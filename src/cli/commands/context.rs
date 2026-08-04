use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::response::{
    CallHierarchyOutput, LocationOutput, RefOutput, Section, TargetOutput, TestOutput,
    TypeInfoOutput,
};
use crate::cli::utils::{extract_signature, find_symbol_at_position};
use crate::cli::{LocationArg, OutputError};

use super::common::lsp_error_at;
use crate::models::lsp::{CallHierarchyItem, FindSymbolsOptions};
use crate::models::symbol::Symbol;
use crate::services::TestScope;
use crate::services::lsp::LspService;
use crate::utils::estimate_tokens;

#[derive(Args, Debug)]
pub struct ContextArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Include all context (callers, callees, types, tests)
    #[arg(short, long)]
    pub all: bool,

    /// Include callers (incoming calls)
    #[arg(long)]
    pub callers: bool,

    /// Include callees (outgoing calls)
    #[arg(long)]
    pub callees: bool,

    /// Include type definitions used
    #[arg(long)]
    pub types: bool,

    /// Include related tests (detected by file patterns)
    #[arg(long)]
    pub tests: bool,

    /// Include source body of target symbol
    #[arg(long)]
    pub body: bool,

    /// Attach complete verbatim bodies: the target's whole body
    /// unbudgeted, then callees in listed order and types under
    /// --body-tokens (target-only when no section is requested)
    #[arg(long)]
    pub with_bodies: bool,

    /// Token budget for the callee/type bodies attached by --with-bodies
    /// (whole-body-or-nothing per item; inert without --with-bodies)
    #[arg(long, default_value_t = crate::constants::defaults::CONTEXT_BODY_TOKENS)]
    pub body_tokens: usize,
}

/// Context response with pure fact data
#[derive(Debug, Serialize)]
pub struct ContextOutput {
    /// Target symbol information
    pub target: TargetOutput,
    /// Reference summary (pure fact)
    pub refs: RefOutput,
    /// Callers (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers: Option<Section<CallHierarchyOutput>>,
    /// Callees (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callees: Option<Section<CallHierarchyOutput>>,
    /// Type definitions (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub types: Option<Section<TypeInfoOutput>>,
    /// Related tests (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<Section<TestOutput>>,
}

struct ContextParams {
    calls: usize,
    tests: usize,
}

pub async fn execute(args: ContextArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = args.loc.parse()?.to_absolute()?;
    let config = app.config();
    let params = ContextParams {
        calls: config.lsp.calls_limit,
        tests: config.lsp.tests_limit,
    };

    let analysis = match LocationAnalysis::at(app.lsp.as_ref(), loc.clone(), ctx.root()).await {
        Ok(a) => a,
        Err(e) => {
            ctx.print_error(lsp_error_at(
                e,
                &ctx.relative_path(&loc.file),
                loc.line,
                loc.column,
            ));
            return Ok(());
        }
    };

    let response = fetch_context(
        app.lsp.as_ref(),
        &args,
        ctx.root(),
        app.test_scope(),
        &params,
        analysis,
    )
    .await;

    ctx.print_success(response);
    Ok(())
}

async fn fetch_context(
    lsp: &dyn LspService,
    args: &ContextArgs,
    root: &Path,
    test_scope: &TestScope,
    params: &ContextParams,
    analysis: LocationAnalysis,
) -> ContextOutput {
    let resolved_line = analysis
        .target
        .as_ref()
        .map(|s| s.location.line)
        .unwrap_or(analysis.anchor.line);
    let resolved_column = analysis
        .target
        .as_ref()
        .map(|s| s.location.column)
        .unwrap_or(analysis.anchor.column);

    let target = {
        let mut t = TargetOutput::from_symbol_or_fallback(
            analysis.target.as_ref(),
            &analysis.anchor.file,
            resolved_line,
            resolved_column,
            root,
            analysis.anchor_resolution().as_status(),
        );
        if let Some(sym) = analysis.target.as_ref() {
            t = t.with_signature(extract_signature(sym.body.as_deref()));
            if args.body || args.all || args.with_bodies {
                t = t.with_body(sym.body.clone());
            }
        }
        t
    };

    // `refs` here is a summary, not a list — nothing to cap, and `total`
    // is the true total the output contract promises.
    let classified = analysis.classify(test_scope);

    let refs_summary = RefOutput {
        total: classified.total,
        test: classified.test,
        prod: classified.prod,
        files: Some(classified.unique_files),
        modules: Some(classified.unique_modules),
        is_exported: analysis.is_exported(),
        // Disclose a warming-index lower bound on the same query the `refs`
        // command discloses it for; the summary counts are otherwise read as
        // authoritative.
        indexing: analysis.indexing(),
    };

    let file = analysis.anchor.file.as_path();

    // Fetch optional sections in parallel
    let want_callers = args.callers || args.all;
    let want_callees = args.callees || args.all;
    let want_types = args.types || args.all;
    let want_tests = args.tests || args.all;

    let (callers, mut callees, mut types, tests) = tokio::join!(
        async {
            if want_callers {
                Some(
                    fetch_calls(
                        lsp,
                        file,
                        resolved_line,
                        resolved_column,
                        root,
                        params.calls,
                        true,
                    )
                    .await,
                )
            } else {
                None
            }
        },
        async {
            if want_callees {
                Some(
                    fetch_calls(
                        lsp,
                        file,
                        resolved_line,
                        resolved_column,
                        root,
                        params.calls,
                        false,
                    )
                    .await,
                )
            } else {
                None
            }
        },
        async {
            if want_types {
                Some(fetch_types(lsp, file, resolved_line, resolved_column, root).await)
            } else {
                None
            }
        },
        async {
            if want_tests {
                Some(fetch_tests(lsp, &classified.test_refs, root, params.tests).await)
            } else {
                None
            }
        },
    );

    if args.with_bodies {
        apply_section_bodies(
            lsp,
            root,
            args.body_tokens,
            callees.as_mut(),
            types.as_mut(),
        )
        .await;
    }

    ContextOutput {
        target,
        refs: refs_summary,
        callers,
        callees,
        types,
        tests,
    }
}

/// Attach complete verbatim bodies to the callee items (in displayed
/// order) and the type item, whole-body-or-nothing under `budget_tokens`
/// — callees first, then types from whatever budget remains. Sequential
/// by design: admission order is the visible item order, so an agent can
/// correlate every omission to an item it sees. Each qualifying section
/// gets `bodies_included` (the count of items that carry a body); an
/// item left without one was omitted because the budget ran out, the
/// symbol was unresolvable at its position, or it genuinely has no body
/// — disclosed, never silent.
async fn apply_section_bodies(
    lsp: &dyn LspService,
    root: &Path,
    budget_tokens: usize,
    callees: Option<&mut Section<CallHierarchyOutput>>,
    types: Option<&mut Section<TypeInfoOutput>>,
) {
    let mut remaining = budget_tokens;
    let mut symbols_by_file: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();

    if let Some(section) = callees
        && section.error.is_none()
        && section.showing > 0
    {
        let mut candidates = Vec::with_capacity(section.items.len());
        for item in &section.items {
            // Joining an already-absolute path replaces the base, so
            // out-of-root locations (emitted absolute) resolve too.
            let file = root.join(&item.location.file);
            candidates.push(
                resolve_body(
                    lsp,
                    &file,
                    item.location.line,
                    item.location.column,
                    &item.name,
                    &mut symbols_by_file,
                )
                .await,
            );
        }
        let (bodies, admitted) = fit_bodies_to_budget(candidates, &mut remaining);
        for (item, body) in section.items.iter_mut().zip(bodies) {
            item.body = body;
        }
        section.bodies_included = Some(admitted);
    }

    if let Some(section) = types
        && section.error.is_none()
        && section.showing > 0
    {
        let mut candidates = Vec::with_capacity(section.items.len());
        for item in &section.items {
            let file = root.join(&item.location.file);
            candidates.push(
                resolve_body(
                    lsp,
                    &file,
                    item.location.line,
                    item.location.column,
                    &item.name,
                    &mut symbols_by_file,
                )
                .await,
            );
        }
        let (bodies, admitted) = fit_bodies_to_budget(candidates, &mut remaining);
        for (item, body) in section.items.iter_mut().zip(bodies) {
            item.body = body;
        }
        section.bodies_included = Some(admitted);
    }
}

/// Resolve the complete tree-sitter body of the symbol at `line:column`
/// in `file`, memoizing one `find_symbols` call per file (a failed fetch
/// memoizes an empty tree — one attempt per file). Column-precise
/// resolution falls back to line-only, then the resolved symbol must
/// match `expected_name` — a mismatch signals position drift, where the
/// body would belong to a different symbol, so it fails closed to
/// omission rather than attaching a plausible-but-wrong body.
async fn resolve_body(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    expected_name: &str,
    symbols_by_file: &mut HashMap<PathBuf, Vec<Symbol>>,
) -> Option<String> {
    if !symbols_by_file.contains_key(file) {
        let symbols = lsp
            .find_symbols(file, FindSymbolsOptions::default().with_body())
            .await
            .unwrap_or_default();
        symbols_by_file.insert(file.to_path_buf(), symbols);
    }
    let symbols = &symbols_by_file[file];
    let symbol = find_symbol_at_position(symbols, line, Some(column))
        .or_else(|| find_symbol_at_position(symbols, line, None))?;
    if !is_same_symbol_name(expected_name, &symbol.name) {
        return None;
    }
    symbol.body.clone().filter(|b| !b.is_empty())
}

/// Greedy whole-body-or-nothing admission in candidate order: an
/// oversized body is skipped — to None — and smaller later bodies may
/// still admit, so one outlier can't starve the rest. The stated budget
/// is never exceeded (no admit-first override: a context response stays
/// fully useful without bodies). Returns the positionally aligned
/// admitted bodies and how many were admitted; `remaining` carries the
/// leftover budget to the next section.
fn fit_bodies_to_budget(
    candidates: Vec<Option<String>>,
    remaining: &mut usize,
) -> (Vec<Option<String>>, usize) {
    let mut admitted = 0;
    let bodies = candidates
        .into_iter()
        .map(|candidate| {
            let body = candidate?;
            let cost = estimate_tokens(&body);
            if cost <= *remaining {
                *remaining -= cost;
                admitted += 1;
                Some(body)
            } else {
                None
            }
        })
        .collect();
    (bodies, admitted)
}

/// True when the two names denote the same symbol: exact, or one is the
/// `::`/`.`-qualified form of the other (e.g. `Type.method` vs `method`).
/// Both names come from the same language server, so a mismatch means
/// the position resolved to a different symbol.
fn is_same_symbol_name(item_name: &str, symbol_name: &str) -> bool {
    if item_name == symbol_name {
        return true;
    }
    let qualified_tail = |qualified: &str, tail: &str| {
        qualified.ends_with(&format!("::{tail}")) || qualified.ends_with(&format!(".{tail}"))
    };
    qualified_tail(item_name, symbol_name) || qualified_tail(symbol_name, item_name)
}

async fn fetch_calls(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    root: &Path,
    limit: usize,
    incoming: bool,
) -> Section<CallHierarchyOutput> {
    let result: Result<crate::models::lsp::Indexed<Vec<CallHierarchyItem>>, _> = if incoming {
        lsp.incoming_calls(file, line, column).await
    } else {
        lsp.outgoing_calls(file, line, column).await
    };

    match result {
        Ok(calls) => {
            let total = calls.data.len();
            let items: Vec<CallHierarchyOutput> = calls
                .data
                .iter()
                .take(limit)
                .map(|c| CallHierarchyOutput::from_item(c, root))
                .collect();
            let file_rel = file
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| file.display().to_string());
            Section::with_total(items, total)
                .with_next_commands(call_hierarchy_next_commands(
                    incoming, &file_rel, line, column, total, limit,
                ))
                // The same computation-time marker every cross-file
                // section carries: a cold-start caller/callee list is a
                // lower bound and must say so.
                .with_indexing(calls.indexing)
        }
        Err(e) => Section::error(format_call_hierarchy_error(
            &e.to_string(),
            file,
            line,
            column,
        )),
    }
}

/// Truncation-only steering for a callers/callees section: the per-call
/// cap is config-only (`lsp.calls_limit` has no `context` flag), so the
/// standalone command with `--limit <total>` is the one runnable way to
/// see the complete list. Complete sections emit nothing.
fn call_hierarchy_next_commands(
    incoming: bool,
    file_rel: &str,
    line: u32,
    column: u32,
    total: usize,
    limit: usize,
) -> Vec<String> {
    if total > limit {
        vec![format!(
            "symora {} {file_rel}:{line}:{column} --limit {total}",
            if incoming { "callers" } else { "callees" }
        )]
    } else {
        Vec::new()
    }
}

async fn fetch_types(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    root: &Path,
) -> Section<TypeInfoOutput> {
    match lsp.goto_type_definition(file, line, column).await {
        Ok(Some(type_loc)) => {
            let type_symbols = lsp
                .find_symbols(&type_loc.file, FindSymbolsOptions::default())
                .await
                .unwrap_or_default();

            let type_sym = find_symbol_at_position(&type_symbols, type_loc.line, None);
            let items = vec![TypeInfoOutput {
                name: type_sym
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                kind: type_sym
                    .map(|s| s.kind.to_string())
                    .unwrap_or_else(|| "type".to_string()),
                location: LocationOutput::from_location(&type_loc, root),
                detail: None,
                body: None,
            }];
            Section::new(items)
        }
        Ok(None) => Section::new(vec![]),
        Err(e) => Section::error(format_type_error(&e.to_string(), file, line, column)),
    }
}

fn format_call_hierarchy_error(error: &str, file: &Path, line: u32, column: u32) -> OutputError {
    if is_unsupported_lsp_feature(error) {
        OutputError::unsupported("Call hierarchy is not supported at this position").with_hint(
            format!(
                "Use `symora refs {}:{}:{}` for usages or `symora usage {}:{}:{}` for broader symbol analysis.",
                file.display(),
                line,
                column,
                file.display(),
                line,
                column,
            ),
        )
    } else {
        OutputError::internal(error.to_string())
    }
}

fn format_type_error(error: &str, file: &Path, line: u32, column: u32) -> OutputError {
    if is_unsupported_lsp_feature(error) {
        OutputError::unsupported("Type definition lookup is not supported at this position")
            .with_hint(format!(
                "Use `symora hover {}:{}:{}` or inspect the target body in this context output instead.",
                file.display(),
                line,
                column,
            ))
    } else {
        OutputError::internal(error.to_string())
    }
}

fn is_unsupported_lsp_feature(error: &str) -> bool {
    error.contains("does not support") || error.contains("no handler for request")
}

/// Name each covering test by resolving the symbol that ENCLOSES the
/// reference, from the language server's own symbol tree.
///
/// The alternative — scanning backwards for annotation text — cannot tell
/// `test(` in a spec file from a call to a function named `test`, and every
/// language needs its own vocabulary of markers to guess with. Asking which
/// symbol contains the line is one question, exact for every language that
/// serves document symbols, and it names the test that actually covers the
/// usage rather than the nearest thing that looked like one.
async fn fetch_tests(
    lsp: &dyn LspService,
    test_refs: &[&crate::models::symbol::Location],
    root: &Path,
    limit: usize,
) -> Section<TestOutput> {
    let mut items = Vec::with_capacity(limit);
    let mut symbols_by_file: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();

    for r in test_refs.iter().take(limit) {
        let symbols = match symbols_by_file.entry(r.file.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(
                lsp.find_symbols(&r.file, FindSymbolsOptions::default().with_depth(10))
                    .await
                    .unwrap_or_default(),
            ),
        };

        if let Some(symbol) = find_symbol_at_position(symbols, r.line, Some(r.column)) {
            items.push(TestOutput {
                name: symbol
                    .name_path
                    .clone()
                    .unwrap_or_else(|| symbol.name.clone()),
                location: LocationOutput::from_location(r, root),
            });
        }
    }

    Section::new(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;

    use crate::error::LspError;
    use crate::models::lsp::{
        ApplyActionResult, CodeAction, CodeLens, FoldingRange, HoverInfo, InlayHint,
        PrepareRenameResult, RenameResult, SelectionRange, ServerStatus, SignatureHelp, TextEdit,
        TypeHierarchyItem,
    };
    use crate::models::symbol::{Language, Location, SymbolKind};

    #[test]
    fn truncated_callers_steer_to_callers_with_limit() {
        assert_eq!(
            call_hierarchy_next_commands(true, "f.rs", 12, 4, 12, 8),
            vec!["symora callers f.rs:12:4 --limit 12"]
        );
    }

    #[test]
    fn complete_calls_emit_nothing() {
        assert!(call_hierarchy_next_commands(true, "f.rs", 12, 4, 8, 8).is_empty());
        assert!(call_hierarchy_next_commands(false, "f.rs", 12, 4, 0, 8).is_empty());
    }

    #[test]
    fn callees_use_callees_command() {
        assert_eq!(
            call_hierarchy_next_commands(false, "f.rs", 12, 4, 9, 8),
            vec!["symora callees f.rs:12:4 --limit 9"]
        );
    }

    #[test]
    fn same_symbol_name_accepts_exact_and_qualified_tails() {
        assert!(is_same_symbol_name("process", "process"));
        assert!(is_same_symbol_name("Handler::process", "process"));
        assert!(is_same_symbol_name("process", "Handler::process"));
        assert!(is_same_symbol_name("Handler.process", "process"));
        assert!(is_same_symbol_name("process", "Handler.process"));
    }

    #[test]
    fn same_symbol_name_rejects_different_symbols() {
        assert!(!is_same_symbol_name("process", "prepare"));
        assert!(!is_same_symbol_name("Handler::process", "prepare"));
        // A bare suffix without a qualification separator is a different
        // identifier, not a qualified form.
        assert!(!is_same_symbol_name("reprocess", "process"));
        assert!(!is_same_symbol_name("process", "reprocess"));
    }

    #[test]
    fn budget_fit_admits_an_exact_fit_boundary() {
        // 8 chars = exactly 2 tokens under the 4-chars-per-token estimate.
        let mut remaining = 2;
        let (bodies, admitted) =
            fit_bodies_to_budget(vec![Some("12345678".to_string())], &mut remaining);
        assert_eq!(bodies, vec![Some("12345678".to_string())]);
        assert_eq!(admitted, 1);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn budget_fit_skips_an_oversized_body_and_admits_a_smaller_later_one() {
        let big = "x".repeat(100); // 25 tokens
        let small = "y".repeat(20); // 5 tokens
        let mut remaining = 10;
        let (bodies, admitted) =
            fit_bodies_to_budget(vec![Some(big), Some(small.clone())], &mut remaining);
        assert_eq!(bodies, vec![None, Some(small)]);
        assert_eq!(admitted, 1);
        assert_eq!(remaining, 5);
    }

    #[test]
    fn budget_fit_shares_remaining_across_sequential_calls() {
        let mut remaining = 12;
        let (_, first) = fit_bodies_to_budget(vec![Some("a".repeat(40))], &mut remaining); // 10 tokens
        assert_eq!(first, 1);
        assert_eq!(remaining, 2);
        // The second (types) call only sees what the first left over.
        let (bodies, second) = fit_bodies_to_budget(vec![Some("b".repeat(40))], &mut remaining);
        assert_eq!(bodies, vec![None]);
        assert_eq!(second, 0);
        assert_eq!(remaining, 2);
    }

    #[test]
    fn budget_fit_passes_unresolved_candidates_through() {
        let mut remaining = 100;
        let (bodies, admitted) = fit_bodies_to_budget(vec![None, None], &mut remaining);
        assert_eq!(bodies, vec![None, None]);
        assert_eq!(admitted, 0);
        assert_eq!(remaining, 100);
    }

    #[test]
    fn budget_fit_admits_nothing_under_a_zero_budget() {
        let mut remaining = 0;
        let (bodies, admitted) =
            fit_bodies_to_budget(vec![Some("fn f() {}".to_string())], &mut remaining);
        assert_eq!(bodies, vec![None]);
        assert_eq!(admitted, 0);
    }

    /// Body-lookup stub: serves canned per-file `documentSymbol` trees
    /// with bodies. Every other `LspService` method is unreachable from
    /// `apply_section_bodies` and panics loudly if that ever changes.
    struct BodyLookupStub {
        symbols_by_file: HashMap<PathBuf, Vec<Symbol>>,
    }

    fn body_symbol(name: &str, file: &Path, line: u32, end_line: u32, body: &str) -> Symbol {
        Symbol::new(
            name.to_string(),
            SymbolKind::Function,
            Location::full(file.to_path_buf(), line, 1, line, 1, end_line, 1),
        )
        .with_body(body.to_string())
    }

    #[async_trait]
    impl LspService for BodyLookupStub {
        async fn find_symbols(
            &self,
            file: &Path,
            _options: FindSymbolsOptions,
        ) -> Result<Vec<Symbol>, LspError> {
            self.symbols_by_file
                .get(file)
                .cloned()
                .ok_or_else(|| LspError::server_error_friendly(-1, "no symbols".to_string()))
        }

        async fn workspace_symbols(
            &self,
            _query: &str,
            _language: Language,
        ) -> Result<crate::models::lsp::Indexed<Vec<Symbol>>, LspError> {
            unreachable!()
        }
        async fn find_references(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<crate::models::lsp::Indexed<Vec<Location>>, LspError> {
            unreachable!()
        }
        async fn goto_definition(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<Location>, LspError> {
            unreachable!()
        }
        async fn goto_type_definition(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<Location>, LspError> {
            unreachable!()
        }
        async fn find_implementations(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<crate::models::lsp::Indexed<Vec<Location>>, LspError> {
            unreachable!()
        }
        async fn hover(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<HoverInfo>, LspError> {
            unreachable!()
        }
        async fn signature_help(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<SignatureHelp>, LspError> {
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
        ) -> Result<Option<PrepareRenameResult>, LspError> {
            unreachable!()
        }
        async fn rename(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
            _new_name: &str,
        ) -> Result<RenameResult, LspError> {
            unreachable!()
        }
        async fn incoming_calls(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<crate::models::lsp::Indexed<Vec<CallHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn outgoing_calls(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<crate::models::lsp::Indexed<Vec<CallHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn supertypes(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<crate::models::lsp::Indexed<Vec<TypeHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn subtypes(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<crate::models::lsp::Indexed<Vec<TypeHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn inlay_hints(
            &self,
            _file: &Path,
            _start_line: u32,
            _end_line: u32,
        ) -> Result<Vec<InlayHint>, LspError> {
            unreachable!()
        }
        async fn folding_ranges(&self, _file: &Path) -> Result<Vec<FoldingRange>, LspError> {
            unreachable!()
        }
        async fn selection_ranges(
            &self,
            _file: &Path,
            _positions: Vec<(u32, u32)>,
        ) -> Result<Vec<SelectionRange>, LspError> {
            unreachable!()
        }
        async fn code_lenses(&self, _file: &Path) -> Result<Vec<CodeLens>, LspError> {
            unreachable!()
        }
        async fn code_actions(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<CodeAction>, LspError> {
            unreachable!()
        }
        async fn apply_code_action(
            &self,
            _file: &Path,
            _action: &CodeAction,
        ) -> Result<ApplyActionResult, LspError> {
            unreachable!()
        }
        async fn format(&self, _file: &Path) -> Result<Vec<TextEdit>, LspError> {
            unreachable!()
        }
        async fn is_available(&self, _language: Language) -> bool {
            unreachable!()
        }
        async fn server_status(&self, _language: Language) -> ServerStatus {
            unreachable!()
        }
    }

    fn callee_item(name: &str, file: &str, line: u32) -> CallHierarchyOutput {
        CallHierarchyOutput {
            name: name.to_string(),
            location: LocationOutput {
                file: file.to_string(),
                line,
                column: 1,
                snippet: None,
                degraded_column: None,
            },
            call_site: None,
            body: None,
        }
    }

    fn type_item(name: &str, file: &str, line: u32) -> TypeInfoOutput {
        TypeInfoOutput {
            name: name.to_string(),
            kind: "struct".to_string(),
            location: LocationOutput {
                file: file.to_string(),
                line,
                column: 1,
                snippet: None,
                degraded_column: None,
            },
            detail: None,
            body: None,
        }
    }

    fn stub_with(symbols: Vec<(&str, Vec<Symbol>)>) -> BodyLookupStub {
        BodyLookupStub {
            symbols_by_file: symbols
                .into_iter()
                .map(|(file, syms)| (PathBuf::from(file), syms))
                .collect(),
        }
    }

    #[tokio::test]
    async fn bodies_attach_in_display_order_with_disclosed_count() {
        let root = Path::new("/repo");
        let file = Path::new("/repo/src/a.rs");
        let stub = stub_with(vec![(
            "/repo/src/a.rs",
            vec![
                body_symbol("alpha", file, 10, 12, "fn alpha() { 1 }"),
                body_symbol("beta", file, 20, 22, "fn beta() { 2 }"),
            ],
        )]);
        let mut callees = Some(Section::new(vec![
            callee_item("alpha", "src/a.rs", 10),
            callee_item("beta", "src/a.rs", 20),
        ]));

        apply_section_bodies(&stub, root, 1000, callees.as_mut(), None).await;

        let section = callees.unwrap();
        assert_eq!(section.items[0].body.as_deref(), Some("fn alpha() { 1 }"));
        assert_eq!(section.items[1].body.as_deref(), Some("fn beta() { 2 }"));
        assert_eq!(section.bodies_included, Some(2));
    }

    #[tokio::test]
    async fn unresolvable_file_omits_its_body_and_the_pass_continues() {
        let root = Path::new("/repo");
        let file = Path::new("/repo/src/a.rs");
        let stub = stub_with(vec![(
            "/repo/src/a.rs",
            vec![body_symbol("alpha", file, 10, 12, "fn alpha() { 1 }")],
        )]);
        let mut callees = Some(Section::new(vec![
            callee_item("ghost", "src/missing.rs", 5),
            callee_item("alpha", "src/a.rs", 10),
        ]));

        apply_section_bodies(&stub, root, 1000, callees.as_mut(), None).await;

        let section = callees.unwrap();
        assert_eq!(section.items[0].body, None);
        assert_eq!(section.items[1].body.as_deref(), Some("fn alpha() { 1 }"));
        assert_eq!(section.bodies_included, Some(1));
    }

    #[tokio::test]
    async fn name_mismatch_fails_closed_to_disclosed_omission() {
        let root = Path::new("/repo");
        let file = Path::new("/repo/src/a.rs");
        let stub = stub_with(vec![(
            "/repo/src/a.rs",
            vec![body_symbol("alpha", file, 10, 12, "fn alpha() { 1 }")],
        )]);
        // The item claims a different symbol at alpha's position — stale
        // index drift; attaching alpha's body would be plausible-but-wrong.
        let mut callees = Some(Section::new(vec![callee_item("other", "src/a.rs", 10)]));

        apply_section_bodies(&stub, root, 1000, callees.as_mut(), None).await;

        let section = callees.unwrap();
        assert_eq!(section.items[0].body, None);
        assert_eq!(section.bodies_included, Some(0));
    }

    #[tokio::test]
    async fn zero_budget_still_discloses_that_attachment_ran() {
        let root = Path::new("/repo");
        let file = Path::new("/repo/src/a.rs");
        let stub = stub_with(vec![(
            "/repo/src/a.rs",
            vec![body_symbol("alpha", file, 10, 12, "fn alpha() { 1 }")],
        )]);
        let mut callees = Some(Section::new(vec![callee_item("alpha", "src/a.rs", 10)]));

        apply_section_bodies(&stub, root, 0, callees.as_mut(), None).await;

        let section = callees.unwrap();
        assert_eq!(section.items[0].body, None);
        assert_eq!(section.bodies_included, Some(0));
    }

    #[tokio::test]
    async fn errored_section_gets_no_bodies_included() {
        let root = Path::new("/repo");
        let stub = stub_with(vec![]);
        let mut callees: Option<Section<CallHierarchyOutput>> = Some(Section::error(
            OutputError::unsupported("no call hierarchy"),
        ));

        apply_section_bodies(&stub, root, 1000, callees.as_mut(), None).await;

        assert_eq!(callees.unwrap().bodies_included, None);
    }

    #[tokio::test]
    async fn types_draw_only_the_budget_callees_left_over() {
        let root = Path::new("/repo");
        let a = Path::new("/repo/src/a.rs");
        let t = Path::new("/repo/src/t.rs");
        let callee_body = "a".repeat(40); // 10 tokens
        let type_body = "t".repeat(40); // 10 tokens — over the 2 left
        let stub = stub_with(vec![
            (
                "/repo/src/a.rs",
                vec![body_symbol("alpha", a, 10, 12, &callee_body)],
            ),
            (
                "/repo/src/t.rs",
                vec![body_symbol("MyType", t, 5, 9, &type_body)],
            ),
        ]);
        let mut callees = Some(Section::new(vec![callee_item("alpha", "src/a.rs", 10)]));
        let mut types = Some(Section::new(vec![type_item("MyType", "src/t.rs", 5)]));

        apply_section_bodies(&stub, root, 12, callees.as_mut(), types.as_mut()).await;

        let callees = callees.unwrap();
        assert_eq!(callees.items[0].body.as_deref(), Some(callee_body.as_str()));
        assert_eq!(callees.bodies_included, Some(1));
        let types = types.unwrap();
        assert_eq!(types.items[0].body, None);
        assert_eq!(types.bodies_included, Some(0));
    }

    #[tokio::test]
    async fn type_body_attaches_when_budget_remains() {
        let root = Path::new("/repo");
        let t = Path::new("/repo/src/t.rs");
        let stub = stub_with(vec![(
            "/repo/src/t.rs",
            vec![body_symbol("MyType", t, 5, 9, "struct MyType { x: u32 }")],
        )]);
        let mut types = Some(Section::new(vec![type_item("MyType", "src/t.rs", 5)]));

        apply_section_bodies(&stub, root, 1000, None, types.as_mut()).await;

        let types = types.unwrap();
        assert_eq!(
            types.items[0].body.as_deref(),
            Some("struct MyType { x: u32 }")
        );
        assert_eq!(types.bodies_included, Some(1));
    }
}
