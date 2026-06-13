use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::Section;
use crate::cli::utils::{SymbolResolution, column_addressed_symbol, line_addressed_symbol};
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::services::lsp::LspService;

/// Why a `SnappedAnchor` did or did not resolve to a symbol. The two failure
/// cases are kept distinct because they license different claims: `NotASymbol`
/// was checked and is genuinely not a symbol, so an empty downstream answer is
/// authoritatively "not a symbol"; `Unavailable` could not be checked (the
/// symbol read failed), so neither "is" nor "is not a symbol" may be claimed —
/// only that the position could not be snapped. Collapsing them would let a mere
/// read failure be reported as a false "not a symbol".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnchorResolution {
    /// Snapped to a symbol's name anchor (possibly after disambiguating a
    /// multi-declaration line).
    Resolved,
    /// Symbols were read but none is addressed by this position.
    NotASymbol,
    /// The symbol read failed, so the position could not be snapped; whether it
    /// is a symbol is unknown.
    Unavailable,
}

/// A snapped anchor position plus its disclosure: when a line-only input
/// hit a multi-declaration line, the first declaration was chosen and
/// `hint` names the alternatives.
pub(crate) struct SnappedAnchor {
    pub line: u32,
    pub column: u32,
    pub hint: Option<String>,
    /// How the input resolved — see [`AnchorResolution`]. A caller must not
    /// present a position-derived verdict about a non-`Resolved` anchor as
    /// authoritative.
    pub resolution: AnchorResolution,
}

impl SnappedAnchor {
    /// Whether the input snapped cleanly to a symbol.
    pub(crate) fn is_resolved(&self) -> bool {
        matches!(self.resolution, AnchorResolution::Resolved)
    }

    /// Disclosure hints for this anchor as a list/set query's from-position: the
    /// multi-declaration ambiguity hint (always, when present) plus — only when
    /// `result_empty`, since the guidance is about how to read an EMPTY answer —
    /// a state-specific unresolved marker. `subject` names the result domain
    /// (e.g. "callers", "implementations"). Every symbol-level list surface
    /// routes its disclosure through this one idiom so an empty position-derived
    /// answer is never misread as an authoritative "there are none".
    pub(crate) fn anchor_hints(
        &self,
        relative_path: &str,
        subject: &str,
        result_empty: bool,
    ) -> Vec<String> {
        let mut hints: Vec<String> = self.hint.iter().cloned().collect();
        if result_empty {
            match self.resolution {
                AnchorResolution::Resolved => {}
                AnchorResolution::NotASymbol => hints.push(format!(
                    "from-position {relative_path}:{} is not a symbol; the empty result means \
                     that, not that it has no {subject} — anchor at a declaration (e.g. a \
                     search_symbols result)",
                    self.line,
                )),
                AnchorResolution::Unavailable => hints.push(format!(
                    "from-position {relative_path}:{} could not be read to resolve a symbol, so \
                     it was not snapped to its declaration; the empty {subject} result may be \
                     incomplete — retry, or anchor at a declaration (e.g. a search_symbols \
                     result)",
                    self.line,
                )),
            }
        }
        hints
    }

    /// Disclosure hints for this anchor as a reachability-verdict endpoint (the
    /// `--to` path query): the ambiguity hint (always) plus — when the position
    /// is not a verified symbol — a state-specific marker that the verdict about
    /// it is not authoritative. `role` labels the endpoint, `remedy` how to fix
    /// it. The verdict shape differs from [`anchor_hints`](Self::anchor_hints)
    /// because a `--to` query always returns a verdict, never an empty list; a
    /// non-`Resolved` endpoint never coincides with a `found` verdict, so the
    /// marker can attach unconditionally.
    pub(crate) fn verdict_hints(
        &self,
        role: &str,
        relative_path: &str,
        remedy: &str,
    ) -> Vec<String> {
        let mut hints: Vec<String> = self.hint.iter().cloned().collect();
        match self.resolution {
            AnchorResolution::Resolved => {}
            AnchorResolution::NotASymbol => hints.push(format!(
                "{role} {relative_path}:{} is not a symbol; the reachability verdict is not \
                 authoritative — {remedy}",
                self.line,
            )),
            AnchorResolution::Unavailable => hints.push(format!(
                "{role} {relative_path}:{} could not be read to resolve a symbol; the \
                 reachability verdict is not authoritative — retry, or {remedy}",
                self.line,
            )),
        }
        hints
    }
}

/// Snap an input position to the authoritative name anchor of the symbol
/// it addresses, through the same line/column addressing rules the edit
/// surface uses (`cli::utils::symbol_nav`): an explicit column resolves
/// position-precisely (with the declaration-start fallback that makes a
/// `search symbols` row's `pub`/`fn` column work); an omitted column
/// addresses the symbol DECLARED on the line, body lines falling back to
/// the enclosing symbol. Symbol-level commands — references, callers,
/// callees, implementations, type hierarchy, impact, context — all route
/// through these rules so "the symbol on this line" means the same thing
/// on every surface. Position-exact commands (def, hover, typedef)
/// deliberately do not.
///
/// Ambiguity (several declarations on one line, no column) resolves to
/// the line's first declaration with a disclosure hint — navigation
/// discloses where an edit would refuse.
pub(crate) async fn snap_to_symbol_anchor(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: Option<u32>,
) -> SnappedAnchor {
    let unsnapped = |resolution| SnappedAnchor {
        line,
        column: column.unwrap_or(1),
        hint: None,
        resolution,
    };
    let Ok(symbols) = lsp
        .find_symbols(file, FindSymbolsOptions::default().with_depth(10))
        .await
    else {
        return unsnapped(AnchorResolution::Unavailable);
    };
    let outcome = match column {
        Some(column) => column_addressed_symbol(&symbols, line, column),
        None => line_addressed_symbol(&symbols, line),
    };
    match outcome {
        SymbolResolution::Match(symbol) => SnappedAnchor {
            line: symbol.location.line,
            column: symbol.location.column,
            hint: None,
            resolution: AnchorResolution::Resolved,
        },
        SymbolResolution::Ambiguous(declared) => {
            let names: Vec<&str> = declared.iter().map(|s| s.name.as_str()).collect();
            let first = declared[0];
            SnappedAnchor {
                line: first.location.line,
                column: first.location.column,
                hint: Some(format!(
                    "Line {} declares multiple symbols ({}); resolved to '{}' — pass an \
                     explicit column (file:line:column) to target another",
                    line,
                    names.join(", "),
                    first.name,
                )),
                resolution: AnchorResolution::Resolved,
            }
        }
        SymbolResolution::NotFound => unsnapped(AnchorResolution::NotASymbol),
    }
}

/// Execute a command that returns `Option<T>` from an LSP call.
/// Used by def, typedef, hover, signature.
pub async fn execute_optional<T, O, F, Fut, M, N>(
    app: &App,
    loc: LocationArg,
    lsp_call: F,
    on_found: M,
    on_not_found: N,
) -> Result<()>
where
    F: FnOnce(PathBuf, u32, u32) -> Fut,
    Fut: Future<Output = Result<Option<T>, LspError>>,
    M: FnOnce(T, &crate::cli::output::OutputContext) -> O,
    N: FnOnce() -> O,
    O: Serialize,
{
    let ctx = &app.output;
    let loc = loc.parse()?.to_absolute()?;

    match lsp_call(loc.file, loc.line, loc.column).await {
        Ok(Some(result)) => ctx.print_success(on_found(result, ctx)),
        Ok(None) => ctx.print_success(on_not_found()),
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

/// Execute a command that returns `Indexed<Vec<T>>` from an LSP call,
/// wrapping in `Section`. Used by implementations, callees, supertypes,
/// subtypes — all cross-file graph queries, so each carries the
/// workspace-indexing degradation marker captured when the query ran.
pub async fn execute_list<T, O, F, Fut, M>(
    app: &App,
    loc: LocationArg,
    limit: usize,
    subject: &str,
    lsp_call: F,
    mapper: M,
) -> Result<()>
where
    F: FnOnce(PathBuf, u32, u32) -> Fut,
    Fut: Future<Output = Result<crate::models::lsp::Indexed<Vec<T>>, LspError>>,
    M: Fn(T, &Path) -> O,
    O: Serialize,
{
    let ctx = &app.output;
    let loc = loc.parse()?.to_absolute()?;
    let anchor = snap_to_symbol_anchor(
        app.lsp.as_ref(),
        &loc.file,
        loc.line,
        loc.column_explicit.then_some(loc.column),
    )
    .await;
    let relative = ctx.relative_path(&loc.file);

    match lsp_call(loc.file, anchor.line, anchor.column).await {
        Ok(result) => {
            let total = result.data.len();
            let output: Vec<O> = result
                .data
                .into_iter()
                .take(limit)
                .map(|item| mapper(item, ctx.root()))
                .collect();
            ctx.print_success(
                Section::with_total(output, total)
                    .with_hints(anchor.anchor_hints(&relative, subject, total == 0))
                    .with_indexing(result.indexing),
            );
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(resolution: AnchorResolution, hint: Option<&str>) -> SnappedAnchor {
        SnappedAnchor {
            line: 42,
            column: 1,
            hint: hint.map(str::to_string),
            resolution,
        }
    }

    #[test]
    fn resolved_anchor_with_no_ambiguity_discloses_nothing() {
        assert!(
            anchor(AnchorResolution::Resolved, None)
                .anchor_hints("src/lib.rs", "callers", true)
                .is_empty()
        );
    }

    #[test]
    fn resolved_anchor_keeps_only_the_ambiguity_hint() {
        let hints = anchor(AnchorResolution::Resolved, Some("two declarations here")).anchor_hints(
            "src/lib.rs",
            "callers",
            true,
        );
        assert_eq!(hints, vec!["two declarations here".to_string()]);
    }

    /// A position verified NOT to be a symbol: an empty answer is authoritatively
    /// "not a symbol", and the marker says exactly that — naming the subject and
    /// the from-position so it is actionable.
    #[test]
    fn not_a_symbol_anchor_discloses_on_empty() {
        let hints =
            anchor(AnchorResolution::NotASymbol, None).anchor_hints("src/lib.rs", "callers", true);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("src/lib.rs:42"));
        assert!(hints[0].contains("is not a symbol"));
        assert!(hints[0].contains("no callers"));
    }

    /// Snapping was unavailable (symbols unreadable): the marker must NOT claim
    /// "not a symbol" — only that the position could not be snapped, so an empty
    /// result may be incomplete. A mere read failure is never an authoritative
    /// "not a symbol".
    #[test]
    fn unavailable_anchor_never_claims_not_a_symbol() {
        let hints =
            anchor(AnchorResolution::Unavailable, None).anchor_hints("src/lib.rs", "callees", true);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("could not be read to resolve a symbol"));
        assert!(!hints[0].contains("is not a symbol"));
        assert!(hints[0].contains("callees"));
    }

    /// A non-empty result needs no unresolved disclosure at all — the position
    /// produced an answer, so a "not a symbol" / "incomplete" marker would only
    /// confuse. Only the ambiguity hint (if any) survives.
    #[test]
    fn non_empty_result_suppresses_the_unresolved_marker() {
        assert!(
            anchor(AnchorResolution::NotASymbol, None)
                .anchor_hints("src/lib.rs", "callers", false)
                .is_empty()
        );
        assert!(
            anchor(AnchorResolution::Unavailable, None)
                .anchor_hints("src/lib.rs", "callers", false)
                .is_empty()
        );
    }

    /// The verdict shape distinguishes the same two failure causes, but attaches
    /// unconditionally (a `--to` query always returns a verdict).
    #[test]
    fn verdict_hints_distinguish_not_a_symbol_from_unavailable() {
        assert!(
            anchor(AnchorResolution::Resolved, None)
                .verdict_hints("from-position", "src/lib.rs", "anchor at a declaration")
                .is_empty()
        );

        let not_sym = anchor(AnchorResolution::NotASymbol, None).verdict_hints(
            "--to target",
            "src/lib.rs",
            "point --to at a declaration",
        );
        assert_eq!(not_sym.len(), 1);
        assert!(not_sym[0].contains("--to target src/lib.rs:42 is not a symbol"));
        assert!(not_sym[0].contains("not authoritative"));

        let unavail = anchor(AnchorResolution::Unavailable, None).verdict_hints(
            "from-position",
            "src/lib.rs",
            "anchor at a declaration",
        );
        assert!(unavail[0].contains("could not be read to resolve a symbol"));
        assert!(!unavail[0].contains("is not a symbol"));
    }
}
