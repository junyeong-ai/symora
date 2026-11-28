use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::{Anchor, anchor_at, anchor_on_line, resolve_anchor};
use crate::cli::output::OutputContext;
use crate::cli::response::Section;
use crate::cli::response::disclosure::{Cap, with_cap};
use crate::cli::utils::{
    AnchorResolution, SymbolResolution, Unresolved, ambiguity_hint, line_addressed_symbol,
    shell_arg,
};
use crate::cli::{
    ErrorCode, FileSymbols, LocationArg, OutputError, ParsedLocation, SymbolBackend,
    declared_at_line,
};
use crate::error::LspError;
use crate::models::lsp::{FindSymbolsOptions, Indexed};
use crate::models::symbol::SymbolKind;

/// Read a `--kind` value, naming the kinds it can be.
pub(crate) fn parse_symbol_kind(text: &str) -> Result<SymbolKind, OutputError> {
    text.parse().map_err(|error: String| {
        OutputError::invalid(error).with_hint(format!(
            "Valid kinds: {}",
            SymbolKind::all_kind_names().join(", ")
        ))
    })
}

/// Map a position-anchored LSP failure, adding the recovery route that is
/// still open at that position.
///
/// Which route depends on whether the session can come back. A dropped or
/// timed-out request leaves the server able to serve this workspace, so the
/// advice is to retry and meanwhile read the file's declarations, which the
/// grammar answers if the server still cannot.
/// A session that never established cannot serve it at all, and pointing at
/// another LSP-backed command would send the agent into the same wall — the
/// route there runs through the surfaces that need no language server.
///
/// The split is read from the typed error. Re-deriving it from the rendered
/// message would mean matching prose, which cannot tell the two apart and
/// leaves every unrecognised shape as an `internal` error with no move
/// against it.
pub(crate) fn lsp_error_at(err: LspError, file: &str) -> OutputError {
    let recoverable = err.is_recoverable();
    // A server seen to have stopped reading answers no retry and no other
    // LSP-backed command, and a daemon replaced or stopping mid-command is
    // served again by its successor: each mapped hint names the one way past
    // it.
    let settled = matches!(&err, LspError::Timeout(timed_out) if timed_out.unread.is_some())
        || matches!(
            &err,
            LspError::ConnectionLost(_) | LspError::DaemonReplaced(_) | LspError::DaemonStopping(_)
        );
    let mapped: OutputError = err.into();
    if settled || !matches!(mapped.code, ErrorCode::Timeout | ErrorCode::LspUnavailable) {
        return mapped;
    }
    if recoverable {
        return mapped.with_hint(format!(
            "Retry, or read the file's declarations with `symora symbols -- {}`, which \
             answers from the grammar when the server cannot.",
            shell_arg(file),
        ));
    }
    mapped.with_hint(format!(
        "The server reported why above — resolve that, then retry. \
         Until it can start, `symora map file -- {}` and `symora search content` \
         answer without a language server.",
        shell_arg(file),
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

    /// [`Self::declared_at`], adding which of this tree's own files a
    /// declaration outside the project is another checkout's copy of, where
    /// the relative path proves it ([`crate::cli::python_members::tree_copy_of`]):
    /// a usage here resolving there is the language server reading that copy
    /// in place of this tree's.
    fn declared_at_described(&self, root: &Path) -> String {
        let at = self.declared_at(root);
        match crate::cli::python_members::tree_copy_of(root, &self.file) {
            Some(own) => format!(
                "{at}, another checkout's copy of this tree's {}",
                own.display()
            ),
            None => at,
        }
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
                self.declared_at_described(root),
            )),
            AnchorResolution::Resolved => None,
            AnchorResolution::Binding if self.input_is_anchor() => Some(format!(
                "{at} is a binding the symbol tree does not list (a local, a parameter, a module, a \
                 generated item, or a declaration the server's outline omits); the {subject} shown \
                 are its own"
            )),
            AnchorResolution::Binding => Some(format!(
                "{at} denotes a binding declared at {} that the symbol tree does not list (a \
                 local, a parameter, a module, a generated item, or a declaration the server's \
                 outline omits); the {subject} shown are that binding's",
                self.declared_at_described(root),
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
                 declaration (e.g. a `symora search symbols` result)"
            )),
            AnchorResolution::Unavailable(Unresolved::ReadFailed) => Some(format!(
                "{at} could not be read to resolve a symbol; any {subject} shown are for the raw \
                 position and may be incomplete — retry, or anchor at a declaration (e.g. a \
                 `symora search symbols` result)"
            )),
            AnchorResolution::Unavailable(Unresolved::Loading) => Some(format!(
                "{at} could not be resolved: {LOADING}; any {subject} shown are for the raw \
                 position and may be incomplete — retry once it is warm, or anchor at a \
                 declaration (e.g. a `symora search symbols` result)"
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

    /// [`Self::anchor_hints`] for a query answered by searching the project —
    /// references, callers, implementations — which also says when the
    /// position given lies outside it: such a set holds what was found from
    /// this project, and a zero there says nothing about the file's own.
    pub(crate) fn inbound_hints(&self, root: &Path, subject: &str) -> Vec<String> {
        let mut hints = self.anchor_hints(root, subject);
        hints.extend(self.outside_project_hint(root, subject));
        hints
    }

    pub(crate) fn outside_project_hint(&self, root: &Path, subject: &str) -> Option<String> {
        (!self.input.file.starts_with(root)).then(|| {
            format!(
                "{} lies outside this project ({}), so the {subject} shown are those found from \
                 this project, not from the file's own; run symora from the file's project for \
                 those",
                self.describe(root),
                root.display(),
            )
        })
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
                self.declared_at_described(root),
            )),
            AnchorResolution::Resolved => {}
            AnchorResolution::Binding if self.input_is_anchor() => hints.push(format!(
                "{role} {at} is a binding the symbol tree does not list (a local, a parameter, a module, \
                 a generated item, or a declaration the server's outline omits); the verdict is about \
                 that binding"
            )),
            AnchorResolution::Binding => hints.push(format!(
                "{role} {at} denotes a binding the symbol tree does not list (a local, a \
                 parameter, a module, a generated item, or a declaration the server's outline \
                 omits); the verdict is about that binding, anchored at its declaration {}",
                self.declared_at_described(root),
            )),
            AnchorResolution::NotASymbol => hints.push(format!(
                "{role} {at} is not a symbol; the reachability verdict is not authoritative — \
                 {remedy}",
            )),
            AnchorResolution::Unavailable(Unresolved::ReadFailed) => hints.push(format!(
                "{role} {at} could not be read to resolve a symbol; the reachability verdict \
                 is not authoritative — retry, or {remedy}",
            )),
            AnchorResolution::Unavailable(Unresolved::Loading) => hints.push(format!(
                "{role} {at} could not be resolved: {LOADING}; the reachability verdict is not \
                 authoritative — retry once it is warm, or {remedy}",
            )),
        }
        hints
    }
}

/// What an anchor's resolution met where its language server had not
/// loaded the workspace. The wait is spent from the command's budget, which
/// is what tells a slow start from a position that declares nothing.
const LOADING: &str = "the language server was still loading its workspace when its readiness \
                       wait, which `lsp.timeout_secs` bounds, ran out, and resolved nothing there";

/// Resolve a command's location argument to its anchor — see
/// [`resolve_anchor`]. Symbol-level commands — references, callers, callees,
/// implementations, impact, context — all route through the
/// same rules, so a position means the same thing on every surface; the
/// position-exact commands (def, hover, typedef, rename) read the raw
/// position, and the anchor agrees with them by construction. `options`
/// shape the symbol read.
///
/// A column-less line whose declaration, on it or enclosing it, only the
/// grammar's reading holds ([`declared_at_line`]) addresses that
/// declaration's name as its column would, and an edit the same
/// declaration; the first of several declared on it, with the hint naming
/// the rest.
///
/// A read the resolution needs failing leaves the anchor unavailable only
/// where the failure is the read's own ([`unresolved_by`]); any other is
/// returned, the command's error.
pub(crate) async fn anchor_of(
    app: &App,
    input: &ParsedLocation,
    options: FindSymbolsOptions,
) -> Result<Anchor, LspError> {
    let lsp = app.lsp.as_ref();
    let resolved = if input.column_explicit {
        resolve_anchor(lsp, input, options).await
    } else {
        match declared_at_line(app, &input.file, options.clone(), input.line).await {
            Ok(FileSymbols {
                backend: SymbolBackend::Document,
                symbols,
                ..
            }) => Ok(anchor_on_line(input, &symbols)),
            Ok(FileSymbols {
                symbols,
                served: Some(served),
                ..
            }) => match line_addressed_symbol(&symbols, input.line) {
                SymbolResolution::Match(declared) => {
                    let (line, column) = (declared.location.line, declared.location.column);
                    anchor_at(lsp, input, &served, line, column, options).await
                }
                SymbolResolution::Ambiguous(declared) => {
                    let (line, column) = (declared[0].location.line, declared[0].location.column);
                    anchor_at(lsp, input, &served, line, column, options)
                        .await
                        .map(|anchor| Anchor {
                            hint: Some(ambiguity_hint(input.line, &declared)),
                            ..anchor
                        })
                }
                SymbolResolution::NotFound => Ok(anchor_on_line(input, &served)),
            },
            // The grammar read the file because the server's read failed:
            // that failure is the one the resolution met.
            Ok(FileSymbols {
                unserved: Some(error),
                ..
            })
            | Err(error) => Err(error),
            // A grammar's reading without the server's tree beside it does
            // not say which declaration the server reads at the line.
            Ok(FileSymbols { .. }) => {
                return Ok(Anchor::unavailable(input, Unresolved::ReadFailed));
            }
        }
    };
    match resolved {
        Ok(anchor) => Ok(anchor),
        Err(error) => unresolved_by(error).map(|cause| Anchor::unavailable(input, cause)),
    }
}

/// How a failed read that anchor resolution needed reads, decided by the
/// error's type: as the anchor's [`Unresolved`] cause where the failure is
/// the read's own — the server answered the read with an error about it or
/// with an answer that does not decode, or had not loaded its workspace —
/// and otherwise as the error itself, the command's. The query at the raw
/// position meets every other failure again (the transport or the session
/// lost, the budget spent, no server, capability or readable file to ask),
/// or, where a session started anew answers it, answers for a position
/// nothing resolved, the declaration counted among its own usages.
fn unresolved_by(error: LspError) -> Result<Unresolved, LspError> {
    match error {
        LspError::Indexing { .. } => Ok(Unresolved::Loading),
        LspError::ServerError { .. } if !error.is_recoverable() && !error.is_unsupported() => {
            Ok(Unresolved::ReadFailed)
        }
        LspError::Protocol(_) | LspError::Json(_) => Ok(Unresolved::ReadFailed),
        LspError::ServerError { .. }
        | LspError::ServerStart(_)
        | LspError::NotConnected
        | LspError::ConnectionLost(_)
        | LspError::DaemonReplaced(_)
        | LspError::DaemonStopping(_)
        | LspError::ServerNotInstalled { .. }
        | LspError::UnsupportedLanguage(_)
        | LspError::FeatureNotSupported { .. }
        | LspError::ServerTerminated { .. }
        | LspError::Timeout(_)
        | LspError::RequestCancelled
        | LspError::UnsupportedEdit(_)
        | LspError::FileTooLarge { .. }
        | LspError::FileNotText { .. }
        | LspError::ServerNotConsulted
        | LspError::Io(_) => Err(error),
    }
}

/// Execute a command that returns `Option<T>` from an LSP call.
/// Used by def and hover.
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
    N: FnOnce(Option<crate::models::lsp::IndexingDegradation>) -> O,
    O: Serialize + crate::cli::response::DisclosesIndexing,
{
    let ctx = &app.output;
    let loc = loc.parse()?.resolve(None)?;

    match lsp_call(loc.file, loc.line, loc.column).await {
        Ok(answer) => {
            let output = match answer.data {
                Some(result) => on_found(result, ctx),
                None => on_not_found(answer.indexing),
            };
            ctx.print_success(output.with_indexing(answer.indexing));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

/// Where a list query's answers are found.
#[derive(Clone, Copy)]
pub enum Reach {
    /// Across the project — implementations.
    Inbound,
    /// In the declaration itself — callees.
    Outbound,
}

/// Execute a command that returns `Indexed<Vec<T>>` from an LSP call,
/// wrapping in `Section`. Used by implementations and callees — both
/// cross-file graph queries, so each carries the
/// workspace-indexing degradation marker captured when the query ran.
pub async fn execute_list<T, O, F, Fut, M>(
    app: &App,
    loc: LocationArg,
    limit: usize,
    subject: &str,
    reach: Reach,
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
    let loc = loc.parse()?.resolve(None)?;
    let anchor = match anchor_of(app, &loc, FindSymbolsOptions::default().with_depth(10)).await {
        Ok(anchor) => anchor,
        Err(e) => {
            ctx.print_error(e);
            return Ok(());
        }
    };

    match lsp_call(anchor.file.clone(), anchor.line, anchor.column).await {
        Ok(result) => {
            let total = result.data.len();
            let output: Vec<O> = result
                .data
                .into_iter()
                .take(limit)
                .map(|item| mapper(item, ctx.root()))
                .collect();
            let member = crate::cli::python_members::qualifier(app, &anchor.file);
            let mut hints: Vec<String> = member.iter().map(|q| q.hint.clone()).collect();
            hints.extend(match reach {
                Reach::Inbound => anchor.inbound_hints(ctx.root(), subject),
                Reach::Outbound => anchor.anchor_hints(ctx.root(), subject),
            });
            ctx.print_success(with_cap(
                Section::with_total(output, total)
                    .with_hints(hints)
                    .with_indexing(result.indexing)
                    .with_coverage_gaps(member.iter().map(|q| q.coverage_gap()).collect()),
                Cap::Option("--limit"),
            ));
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

    /// A position in another project is answered from this one: its
    /// references are the ones found from here, and saying so is what keeps
    /// a zero from reading as "nothing uses it". A query read from the
    /// declaration itself — callees — is the file's own answer
    /// wherever it lives, and says nothing.
    #[test]
    fn a_position_outside_the_project_says_whose_answer_it_is() {
        let mut elsewhere = anchor(AnchorResolution::Resolved, None);
        elsewhere.input.file = PathBuf::from("/other/src/lib.rs");
        elsewhere.file = elsewhere.input.file.clone();

        let inbound = elsewhere.inbound_hints(&root(), "references");
        assert_eq!(inbound.len(), 1, "{inbound:?}");
        assert!(inbound[0].contains("outside this project"), "{inbound:?}");
        assert!(elsewhere.anchor_hints(&root(), "callees").is_empty());

        let here = anchor(AnchorResolution::Resolved, None);
        assert!(here.inbound_hints(&root(), "references").is_empty());
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
        let unread = AnchorResolution::Unavailable(Unresolved::ReadFailed);
        let hints = anchor(unread, None).anchor_hints(&root(), "callees");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("could not be read to resolve a symbol"));
        assert!(!hints[0].contains("is not a symbol"));
        assert!(hints[0].contains("callees"));
    }

    /// A server whose readiness wait ran out resolved nothing it had not read
    /// yet: the disclosure says that, and names the budget the wait spent,
    /// rather than a read failure that did not happen.
    #[test]
    fn a_loading_server_is_named_as_the_cause() {
        let loading = AnchorResolution::Unavailable(Unresolved::Loading);
        for hint in [
            anchor(loading, None).anchor_hints(&root(), "references")[0].clone(),
            anchor(loading, None).verdict_hints("--to target", &root(), "point --to at a symbol")
                [0]
            .clone(),
        ] {
            assert!(hint.contains("still loading its workspace"), "{hint}");
            assert!(hint.contains("`lsp.timeout_secs`"), "{hint}");
            assert!(!hint.contains("could not be read"), "{hint}");
            assert!(!hint.contains("is not a symbol"), "{hint}");
        }
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

        let unavail = anchor(AnchorResolution::Unavailable(Unresolved::ReadFailed), None)
            .verdict_hints("from-position", &root(), "anchor at a declaration");
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
                established: false,
            },
            path,
        );
        assert!(matches!(dropped.code, ErrorCode::LspUnavailable));
        let hint = dropped.hint.expect("a dropped session has a retry route");
        assert!(hint.contains("symora symbols"));

        let never_started = lsp_error_at(
            LspError::ServerStart("rust language server: bad workspace".into()),
            path,
        );
        assert!(matches!(never_started.code, ErrorCode::LspUnavailable));
        let hint = never_started
            .hint
            .expect("a rejected handshake still has a route");
        assert!(!hint.contains("symora symbols"));
        assert!(hint.contains("symora map file"));

        // A daemon replaced or stopping under the command is answered by its
        // successor: the cause the shared mapping names is the route.
        for gone in [
            LspError::ConnectionLost("find_symbols".into()),
            LspError::DaemonReplaced("find_symbols".into()),
            LspError::DaemonStopping("find_symbols".into()),
        ] {
            let replaced = lsp_error_at(gone, path);
            assert!(matches!(replaced.code, ErrorCode::LspUnavailable));
            assert_eq!(
                replaced.hint.as_deref(),
                Some("The daemon was stopping or being replaced — retry.")
            );
        }
    }

    /// A position-anchored timeout keeps the restart a server that stopped
    /// reading needs, rather than the retry a slow one does.
    #[test]
    fn a_timeout_where_the_server_stopped_reading_keeps_its_restart() {
        let unread = lsp_error_at(
            LspError::Timeout(crate::error::TimedOut {
                message: "ran out".into(),
                unread: Some(crate::error::UnreadInput {
                    language: crate::models::symbol::Language::Python,
                    secs: 61,
                }),
            }),
            "src/main.py",
        );
        assert!(matches!(unread.code, ErrorCode::Timeout));
        let hint = unread.hint.expect("a stopped server has a remedy");
        assert!(hint.contains("symora daemon restart --project"), "{hint}");

        let slow = lsp_error_at(LspError::timeout("ran out"), "src/main.py");
        let hint = slow.hint.expect("a slow server has a retry route");
        assert!(hint.contains("symora symbols"), "{hint}");
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
        );
        assert!(matches!(unsupported.code, ErrorCode::Unsupported));
        assert_eq!(unsupported.hint.as_deref(), Some("use refs"));
    }

    /// Answers the symbol read with `symbols` where given and otherwise with
    /// `failure`, and every definition with `failure`: the two reads anchor
    /// resolution makes. Nothing else is reachable from `anchor_of`.
    struct Unreadable {
        symbols: Option<Vec<crate::models::symbol::Symbol>>,
        failure: fn() -> LspError,
    }

    #[async_trait::async_trait]
    impl crate::services::lsp::LspService for Unreadable {
        async fn find_symbols(
            &self,
            _file: &Path,
            _options: FindSymbolsOptions,
        ) -> Result<Vec<crate::models::symbol::Symbol>, LspError> {
            self.symbols.clone().ok_or_else(self.failure)
        }
        async fn goto_definition(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Option<crate::models::lsp::Definition>>, LspError> {
            Err((self.failure)())
        }
        async fn workspace_symbols(
            &self,
            _query: &str,
            _language: crate::models::symbol::Language,
        ) -> Result<Indexed<Vec<crate::models::symbol::Symbol>>, LspError> {
            unreachable!()
        }
        async fn find_references(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<crate::models::symbol::Location>>, LspError> {
            unreachable!()
        }
        async fn goto_type_definition(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<crate::models::symbol::Location>>, LspError> {
            unreachable!()
        }
        async fn find_implementations(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<crate::models::symbol::Location>>, LspError> {
            unreachable!()
        }
        async fn hover(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Option<crate::models::lsp::HoverInfo>>, LspError> {
            unreachable!()
        }
        async fn diagnostics(
            &self,
            _file: &Path,
        ) -> Result<crate::models::diagnostic::DiagnosticsReport, LspError> {
            unreachable!()
        }
        async fn prepare_rename(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<crate::models::lsp::PrepareRenameResult>, LspError> {
            unreachable!()
        }
        async fn rename(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
            _new_name: &str,
        ) -> Result<Indexed<Option<crate::models::lsp::RenameResult>>, LspError> {
            unreachable!()
        }
        async fn incoming_calls(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<crate::models::lsp::CallHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn outgoing_calls(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<crate::models::lsp::CallHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn code_actions(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<crate::models::lsp::CodeAction>, LspError> {
            unreachable!()
        }
        async fn apply_code_action(
            &self,
            _file: &Path,
            _action: &crate::models::lsp::CodeAction,
        ) -> Result<crate::models::lsp::ApplyActionResult, LspError> {
            unreachable!()
        }
    }

    /// How each failure of a read anchor resolution needs reads: the read's
    /// own failure leaves the anchor `unavailable` for its cause; any failure
    /// the query at the raw position would meet again, or that a session
    /// started anew would answer for an unresolved position, is the command's
    /// error. Asked of a column-less line, which the grammar still reads, of
    /// an explicit column, whose symbol read fails, and of a column on no
    /// name, whose definition fails.
    #[tokio::test]
    async fn a_failed_resolution_read_is_classified_by_its_type() {
        use crate::infra::lsp::protocol::error_codes;
        use crate::models::symbol::{Language, Location, Symbol};

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, "def alpha():\n    return 1\n").unwrap();
        let alpha = Symbol::new(
            "alpha".to_string(),
            crate::models::symbol::SymbolKind::Function,
            Location::full(file.clone(), 1, 5, 1, 1, 2, 13).with_name_end(1, 10),
        );
        let mut app = App::new_at(
            dir.path().to_path_buf(),
            crate::cli::OutputOptions::default(),
            crate::app::Wiring::default(),
        )
        .await
        .unwrap();

        type Failure = fn() -> LspError;
        let cases: [(&str, Failure, Option<Unresolved>); 11] = [
            (
                "connection lost",
                || LspError::ConnectionLost("find_symbols".into()),
                None,
            ),
            ("not connected", || LspError::NotConnected, None),
            ("budget spent", || LspError::timeout("spent"), None),
            (
                "session dropped",
                || LspError::ServerTerminated {
                    language: Language::Python,
                    established: true,
                },
                None,
            ),
            ("cancelled", || LspError::RequestCancelled, None),
            (
                "content modified",
                || LspError::ServerError {
                    code: error_codes::CONTENT_MODIFIED,
                    message: "content modified".into(),
                },
                None,
            ),
            (
                "unsupported",
                || LspError::ServerError {
                    code: error_codes::METHOD_NOT_FOUND,
                    message: "no such method".into(),
                },
                None,
            ),
            (
                "server unreadable file",
                || LspError::Io(std::io::Error::other("gone")),
                None,
            ),
            (
                "server error",
                || LspError::ServerError {
                    code: error_codes::INTERNAL_ERROR,
                    message: "boom".into(),
                },
                Some(Unresolved::ReadFailed),
            ),
            (
                "undecodable answer",
                || LspError::Protocol("bad".into()),
                Some(Unresolved::ReadFailed),
            ),
            (
                "still indexing",
                || LspError::Indexing {
                    language: Language::Python,
                },
                Some(Unresolved::Loading),
            ),
        ];
        for (case, failure, expected) in cases {
            for (input, symbols) in [
                ((1, 1, false), None),
                ((1, 5, true), None),
                ((2, 5, true), Some(vec![alpha.clone()])),
            ] {
                app.lsp = std::sync::Arc::new(Unreadable { symbols, failure });
                let (line, column, column_explicit) = input;
                let input = ParsedLocation {
                    file: file.clone(),
                    line,
                    column,
                    column_explicit,
                };
                let anchor = anchor_of(&app, &input, FindSymbolsOptions::default()).await;
                match (expected, anchor) {
                    (Some(cause), Ok(anchor)) => {
                        assert_eq!(
                            anchor.resolution,
                            AnchorResolution::Unavailable(cause),
                            "{case} at {line}:{column}"
                        );
                        assert_eq!((anchor.line, anchor.column), (line, column), "{case}");
                    }
                    (None, Err(error)) => {
                        assert_eq!(
                            std::mem::discriminant(&error),
                            std::mem::discriminant(&failure()),
                            "{case} at {line}:{column} came back as another error: {error:?}"
                        );
                    }
                    (expected, anchor) => panic!(
                        "{case} at {line}:{column}: expected {expected:?}, got {:?}",
                        anchor.map(|anchor| anchor.resolution)
                    ),
                }
            }
        }
    }
}
