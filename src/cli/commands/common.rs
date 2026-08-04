use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::Section;
use crate::cli::utils::{
    AnchorResolution, SymbolResolution, ambiguity_hint, column_addressed_symbol,
    line_addressed_symbol,
};
use crate::cli::{ErrorCode, LocationArg, OutputError};
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::services::lsp::LspService;

/// Map a position-anchored LSP failure, adding the recovery route that is
/// still open at that position.
///
/// Which route depends on whether the session can come back. A dropped or
/// timed-out request leaves the server able to serve this workspace, so the
/// advice is to retry and meanwhile fall back to lighter LSP-backed queries.
/// A session that never established cannot serve it at all, and pointing at
/// another LSP-backed command would send the agent into the same wall — the
/// route there runs through the surfaces that need no language server.
///
/// The split is read from the typed error. Re-deriving it from the rendered
/// message would mean matching prose, which cannot tell the two apart and
/// leaves every unrecognised shape as an `internal` error with no move
/// against it.
pub(crate) fn lsp_error_at(err: LspError, file: &str, line: u32, column: u32) -> OutputError {
    let recoverable = err.is_recoverable();
    let mapped: OutputError = err.into();
    if !matches!(mapped.code, ErrorCode::Timeout | ErrorCode::LspUnavailable) {
        return mapped;
    }
    if recoverable {
        return mapped.with_hint(format!(
            "Retry after `symora daemon restart`, or use `symora symbols {file}` and \
             `symora usage {file}:{line}:{column}` to continue from file-level analysis.",
        ));
    }
    mapped.with_hint(format!(
        "The server reported why above — resolve that, then retry. \
         Until it can start, `symora map file {file}` and `symora search content` \
         answer without a language server.",
    ))
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
        self.resolution.is_resolved()
    }

    /// Disclosure hints for this anchor as a list/set query's from-position: the
    /// multi-declaration ambiguity hint (always, when present) plus — whenever
    /// the anchor is not a verified symbol — a state-specific marker, attached
    /// UNCONDITIONALLY (not gated on an empty result). A non-`Resolved` anchor
    /// fell back to the raw `(line, 1)` position, so any rows returned answer a
    /// DIFFERENT question than the user's symbol intent; a populated phantom
    /// result is more misleading than an empty one, never less. `subject` names
    /// the result domain (e.g. "callers", "implementations"). This matches the
    /// unconditional disclosure the verdict, reach, and refs/impact/context
    /// surfaces already make.
    pub(crate) fn anchor_hints(&self, relative_path: &str, subject: &str) -> Vec<String> {
        let mut hints: Vec<String> = self.hint.iter().cloned().collect();
        match self.resolution {
            AnchorResolution::Resolved => {}
            AnchorResolution::NotASymbol => hints.push(format!(
                "from-position {relative_path}:{} did not resolve to a symbol; any {subject} \
                 shown are for the raw position, not a resolved symbol, and an empty result is \
                 not authoritative — anchor at a declaration (e.g. a search_symbols result)",
                self.line,
            )),
            AnchorResolution::Unavailable => hints.push(format!(
                "from-position {relative_path}:{} could not be read to resolve a symbol; any \
                 {subject} shown are for the raw position and may be incomplete — retry, or \
                 anchor at a declaration (e.g. a search_symbols result)",
                self.line,
            )),
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
            let first = declared[0];
            SnappedAnchor {
                line: first.location.line,
                column: first.location.column,
                hint: Some(ambiguity_hint(line, &declared)),
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
                    .with_hints(anchor.anchor_hints(&relative, subject))
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
                .anchor_hints("src/lib.rs", "callers")
                .is_empty()
        );
    }

    #[test]
    fn resolved_anchor_keeps_only_the_ambiguity_hint() {
        let hints = anchor(AnchorResolution::Resolved, Some("two declarations here"))
            .anchor_hints("src/lib.rs", "callers");
        assert_eq!(hints, vec!["two declarations here".to_string()]);
    }

    /// A position verified NOT to be a symbol discloses so, naming the subject
    /// and from-position — UNCONDITIONALLY, since a populated phantom-anchor
    /// result is more misleading than an empty one, never less.
    #[test]
    fn not_a_symbol_anchor_discloses_unconditionally() {
        let hints =
            anchor(AnchorResolution::NotASymbol, None).anchor_hints("src/lib.rs", "callers");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("src/lib.rs:42"));
        assert!(hints[0].contains("did not resolve to a symbol"));
        assert!(hints[0].contains("callers"));
    }

    /// Snapping was unavailable (symbols unreadable): the marker must NOT claim
    /// "not a symbol" — only that the position could not be read. A mere read
    /// failure is never an authoritative "not a symbol".
    #[test]
    fn unavailable_anchor_never_claims_not_a_symbol() {
        let hints =
            anchor(AnchorResolution::Unavailable, None).anchor_hints("src/lib.rs", "callees");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("could not be read to resolve a symbol"));
        assert!(!hints[0].contains("is not a symbol"));
        assert!(hints[0].contains("callees"));
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

    /// The two `lsp_unavailable` causes need opposite advice: a dropped
    /// session can come back, so lighter LSP queries are worth trying; a
    /// session that never started cannot serve any of them.
    #[test]
    fn recovery_route_follows_whether_the_session_can_return() {
        let path = "src/main.rs";

        let dropped = lsp_error_at(
            LspError::ServerTerminated {
                language: crate::models::symbol::Language::Rust,
            },
            path,
            10,
            5,
        );
        assert!(matches!(dropped.code, ErrorCode::LspUnavailable));
        let hint = dropped.hint.expect("a dropped session has a retry route");
        assert!(hint.contains("symora symbols"));

        let never_started = lsp_error_at(
            LspError::ServerStart("rust language server: bad workspace".into()),
            path,
            10,
            5,
        );
        assert!(matches!(never_started.code, ErrorCode::LspUnavailable));
        let hint = never_started
            .hint
            .expect("a rejected handshake still has a route");
        assert!(!hint.contains("symora symbols"));
        assert!(hint.contains("symora map file"));
    }

    /// A capability gap is not a transport failure and keeps the code the
    /// central classifier assigned it.
    #[test]
    fn non_transport_failures_keep_their_own_code() {
        let unsupported = lsp_error_at(
            LspError::FeatureNotSupported {
                language: crate::models::symbol::Language::Rust,
                server: "rust-analyzer".into(),
                feature: "callHierarchy".into(),
                suggestion: "use refs".into(),
            },
            "src/main.rs",
            1,
            1,
        );
        assert!(matches!(unsupported.code, ErrorCode::Unsupported));
        assert_eq!(unsupported.hint.as_deref(), Some("use refs"));
    }
}
