use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::{Anchor, resolve_anchor};
use crate::cli::output::OutputContext;
use crate::cli::response::Section;
use crate::cli::utils::AnchorResolution;
use crate::cli::{ErrorCode, LocationArg, OutputError, ParsedLocation};
use crate::error::LspError;
use crate::models::lsp::{FindSymbolsOptions, Indexed};
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

impl Anchor {
    fn describe(&self, root: &Path) -> String {
        let input = &self.input;
        let path = OutputContext::format_path(&input.file, root);
        if input.column_explicit {
            format!("{path}:{}:{}", input.line, input.column)
        } else {
            format!("{path}:{}", input.line)
        }
    }

    /// The anchor position, relative to the root — the declaration a binding
    /// was resolved to.
    fn declared_at(&self, root: &Path) -> String {
        format!(
            "{}:{}:{}",
            OutputContext::format_path(&self.file, root),
            self.line,
            self.column
        )
    }

    /// Whether the input position is the anchor itself, so there is no
    /// resolved-to declaration to point at.
    fn input_is_anchor(&self) -> bool {
        self.input.file == self.file
            && self.input.line == self.line
            && self.input.column == self.column
    }

    /// The disclosure for how an anchor came to be, in the words of what was
    /// found there: a usage resolved to its declaration, a binding the tree
    /// does not list (the answer is that binding's, anchored at its
    /// declaration), a position that denotes nothing, or a read that failed.
    /// `None` for a symbol addressed directly — the one case with nothing to
    /// disclose.
    fn anchor_disclosure(&self, root: &Path, subject: &str) -> Option<String> {
        let at = self.describe(root);
        let input = &self.input;
        match self.resolution {
            AnchorResolution::Resolved if self.via_definition => Some(format!(
                "{at} is a usage of `{}`, declared at {}; the {subject} shown are its",
                self.symbol
                    .as_ref()
                    .map(|s| s.name.as_str())
                    .unwrap_or_default(),
                self.declared_at(root),
            )),
            AnchorResolution::Resolved => None,
            AnchorResolution::Binding if self.input_is_anchor() => Some(format!(
                "{at} is a binding the symbol tree does not list (a local, a parameter, a module, or a \
                 generated item); the {subject} shown are its own"
            )),
            AnchorResolution::Binding => Some(format!(
                "{at} denotes a binding declared at {} that the symbol tree does not list (a \
                 local, a parameter, a module, or a generated item); the {subject} shown are that \
                 binding's",
                self.declared_at(root),
            )),
            AnchorResolution::NotASymbol if input.column_explicit => Some(format!(
                "{at} is not on a symbol: nothing is declared there and the language server \
                 resolves no definition at it, so any {subject} shown are for the raw position \
                 and an empty result is not authoritative — address the symbol declared on or \
                 enclosing the line with {}:{}, or a declaration from `symora symbols`",
                OutputContext::format_path(&input.file, root),
                input.line,
            )),
            AnchorResolution::NotASymbol => Some(format!(
                "{at} declares no symbol and none encloses it; any {subject} shown are for the \
                 raw position and an empty result is not authoritative — anchor at a \
                 declaration (e.g. a search_symbols result)"
            )),
            AnchorResolution::Unavailable => Some(format!(
                "{at} could not be read to resolve a symbol; any {subject} shown are for the raw \
                 position and may be incomplete — retry, or anchor at a declaration (e.g. a \
                 search_symbols result)"
            )),
        }
    }

    /// Disclosure hints for this anchor as a list/set query's from-position:
    /// the multi-declaration ambiguity hint (always, when present) plus —
    /// whenever the anchor is not a listed symbol — the state-specific
    /// disclosure, attached UNCONDITIONALLY (not gated on an empty result): a
    /// populated result about a raw position is more misleading than an empty
    /// one, never less. `subject` names the result domain (e.g. "callers",
    /// "implementations").
    pub(crate) fn anchor_hints(&self, root: &Path, subject: &str) -> Vec<String> {
        let mut hints: Vec<String> = self.hint.iter().cloned().collect();
        hints.extend(self.anchor_disclosure(root, subject));
        hints
    }

    /// Disclosure hints for this anchor as a reachability-verdict endpoint (the
    /// `--to` path query): the ambiguity hint (always) plus — when the endpoint
    /// is not a listed symbol — a marker that the verdict about it is not
    /// authoritative. `role` labels the endpoint, `remedy` how to fix it. A
    /// `--to` query always returns a verdict, never an empty list; a
    /// non-`Resolved` endpoint never coincides with a `found` verdict, so the
    /// marker can attach unconditionally.
    pub(crate) fn verdict_hints(&self, role: &str, root: &Path, remedy: &str) -> Vec<String> {
        let mut hints: Vec<String> = self.hint.iter().cloned().collect();
        let at = self.describe(root);
        match self.resolution {
            AnchorResolution::Resolved if self.via_definition => hints.push(format!(
                "{role} {at} is a usage of `{}`, declared at {}; the verdict is about that \
                 declaration",
                self.symbol.as_ref().map(|s| s.name.as_str()).unwrap_or_default(),
                self.declared_at(root),
            )),
            AnchorResolution::Resolved => {}
            AnchorResolution::Binding if self.input_is_anchor() => hints.push(format!(
                "{role} {at} is a binding the symbol tree does not list (a local, a parameter, a module, \
                 or a generated item); the verdict is about that binding"
            )),
            AnchorResolution::Binding => hints.push(format!(
                "{role} {at} denotes a binding the symbol tree does not list (a local, a \
                 parameter, a module, or a generated item); the verdict is about that binding, \
                 anchored at its declaration {}",
                self.declared_at(root),
            )),
            AnchorResolution::NotASymbol => hints.push(format!(
                "{role} {at} is not a symbol; the reachability verdict is not authoritative — \
                 {remedy}",
            )),
            AnchorResolution::Unavailable => hints.push(format!(
                "{role} {at} could not be read to resolve a symbol; the reachability verdict \
                 is not authoritative — retry, or {remedy}",
            )),
        }
        hints
    }
}

/// Resolve a command's location argument to its anchor — see
/// [`resolve_anchor`]. Symbol-level commands — references, callers, callees,
/// implementations, type hierarchy, impact, context — all route through the
/// same rules, so a position means the same thing on every surface; the
/// position-exact commands (def, hover, typedef, rename) read the raw
/// position, and the anchor agrees with them by construction.
pub(crate) async fn anchor_of(lsp: &dyn LspService, input: &ParsedLocation) -> Anchor {
    resolve_anchor(lsp, input, FindSymbolsOptions::default().with_depth(10))
        .await
        .unwrap_or_else(|_| Anchor::unavailable(input))
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
    Fut: Future<Output = Result<Indexed<Option<T>>, LspError>>,
    M: FnOnce(T, &crate::cli::output::OutputContext) -> O,
    N: FnOnce() -> O,
    O: Serialize + crate::cli::response::DisclosesIndexing,
{
    let ctx = &app.output;
    let loc = loc.parse()?.to_absolute()?;

    match lsp_call(loc.file, loc.line, loc.column).await {
        Ok(answer) => {
            let output = match answer.data {
                Some(result) => on_found(result, ctx),
                None => on_not_found(),
            };
            ctx.print_success(output.with_indexing(answer.indexing));
        }
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
    let anchor = anchor_of(app.lsp.as_ref(), &loc).await;

    match lsp_call(anchor.file.clone(), anchor.line, anchor.column).await {
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
                    .with_hints(anchor.anchor_hints(ctx.root(), subject))
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

    fn anchor(resolution: AnchorResolution, hint: Option<&str>) -> Anchor {
        let input = ParsedLocation {
            file: PathBuf::from("/repo/src/lib.rs"),
            line: 42,
            column: 1,
            column_explicit: false,
        };
        Anchor {
            file: input.file.clone(),
            line: 42,
            column: 1,
            input,
            symbol: None,
            via_definition: false,
            hint: hint.map(str::to_string),
            resolution,
        }
    }

    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn resolved_anchor_with_no_ambiguity_discloses_nothing() {
        assert!(
            anchor(AnchorResolution::Resolved, None)
                .anchor_hints(&root(), "callers")
                .is_empty()
        );
    }

    #[test]
    fn resolved_anchor_keeps_only_the_ambiguity_hint() {
        let hints = anchor(AnchorResolution::Resolved, Some("two declarations here"))
            .anchor_hints(&root(), "callers");
        assert_eq!(hints, vec!["two declarations here".to_string()]);
    }

    /// A position verified NOT to be a symbol discloses so, naming the subject
    /// and from-position — UNCONDITIONALLY, since a populated phantom-anchor
    /// result is more misleading than an empty one, never less.
    #[test]
    fn not_a_symbol_anchor_discloses_unconditionally() {
        let hints = anchor(AnchorResolution::NotASymbol, None).anchor_hints(&root(), "callers");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("src/lib.rs:42"));
        assert!(hints[0].contains("declares no symbol"));
        assert!(hints[0].contains("callers"));
    }

    /// An explicit column that is on nothing says so in the terms the agent
    /// can act on: what was checked (declaration and definition) and how to
    /// address the line's symbol instead.
    #[test]
    fn a_column_on_nothing_names_the_line_form_as_the_remedy() {
        let mut anchor = anchor(AnchorResolution::NotASymbol, None);
        anchor.input.column = 8;
        anchor.input.column_explicit = true;
        let hints = anchor.anchor_hints(&root(), "references");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("src/lib.rs:42:8 is not on a symbol"));
        assert!(hints[0].contains("resolves no definition"));
        assert!(hints[0].contains("src/lib.rs:42,"));
    }

    /// A position that denotes a binding the tree does not list is anchored at
    /// that binding's declaration, and the disclosure says whose answer this is
    /// — never a re-anchor instruction, since the answer is already exact. When
    /// the position is the binding's own declaration there is nothing to point
    /// at, and the disclosure says so plainly.
    #[test]
    fn a_binding_anchor_discloses_its_declaration() {
        let mut anchor = anchor(AnchorResolution::Binding, None);
        anchor.input.column = 30;
        anchor.input.column_explicit = true;
        anchor.line = 40;
        anchor.column = 9;
        let hints = anchor.anchor_hints(&root(), "references");
        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].contains("src/lib.rs:42:30 denotes a binding declared at src/lib.rs:40:9")
        );
        assert!(hints[0].contains("references shown are that binding's"));
        assert!(!hints[0].contains("not authoritative"));

        let mut own = anchor;
        own.line = 42;
        own.column = 30;
        let hints = own.anchor_hints(&root(), "references");
        assert!(hints[0].contains("src/lib.rs:42:30 is a binding"));
        assert!(!hints[0].contains("declared at"));
    }

    /// Resolution was unavailable (symbols unreadable): the marker must NOT claim
    /// "not a symbol" — only that the position could not be read. A mere read
    /// failure is never an authoritative "not a symbol".
    #[test]
    fn unavailable_anchor_never_claims_not_a_symbol() {
        let hints = anchor(AnchorResolution::Unavailable, None).anchor_hints(&root(), "callees");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("could not be read to resolve a symbol"));
        assert!(!hints[0].contains("is not a symbol"));
        assert!(hints[0].contains("callees"));
    }

    /// A `--to` endpoint that is a binding the tree does not list gets the
    /// verdict about that binding, anchored at its declaration — not the
    /// "not authoritative" marker, since the endpoint is exact.
    #[test]
    fn verdict_hints_name_a_binding_endpoint() {
        let mut anchor = anchor(AnchorResolution::Binding, None);
        anchor.input.column = 30;
        anchor.input.column_explicit = true;
        anchor.line = 40;
        anchor.column = 9;
        let hints = anchor.verdict_hints("--to target", &root(), "point --to at a declaration");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("--to target src/lib.rs:42:30 denotes a binding"));
        assert!(hints[0].contains("anchored at its declaration src/lib.rs:40:9"));
        assert!(!hints[0].contains("not authoritative"));
    }

    /// The verdict shape distinguishes the same failure causes, but attaches
    /// unconditionally (a `--to` query always returns a verdict).
    #[test]
    fn verdict_hints_distinguish_not_a_symbol_from_unavailable() {
        assert!(
            anchor(AnchorResolution::Resolved, None)
                .verdict_hints("from-position", &root(), "anchor at a declaration")
                .is_empty()
        );

        let not_sym = anchor(AnchorResolution::NotASymbol, None).verdict_hints(
            "--to target",
            &root(),
            "point --to at a declaration",
        );
        assert_eq!(not_sym.len(), 1);
        assert!(not_sym[0].contains("--to target src/lib.rs:42 is not a symbol"));
        assert!(not_sym[0].contains("not authoritative"));

        let unavail = anchor(AnchorResolution::Unavailable, None).verdict_hints(
            "from-position",
            &root(),
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
