use std::path::Path;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::response::{
    CallHierarchyOutput, LocationOutput, RefOutput, Section, TargetOutput, TestOutput,
    TypeInfoOutput,
};
use crate::cli::utils::{TestMatcher, extract_signature, find_symbol_at_position};
use crate::cli::{LocationArg, OutputError};
use crate::models::lsp::{CallHierarchyItem, FindSymbolsOptions};
use crate::services::lsp::LspService;

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

struct ContextParams<'a> {
    refs: usize,
    calls: usize,
    tests: usize,
    custom_markers: &'a [String],
}

pub async fn execute(args: ContextArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = args.loc.parse()?.to_absolute()?;
    let config = app.config();
    let params = ContextParams {
        refs: config.lsp.refs_limit,
        calls: config.lsp.calls_limit,
        tests: config.lsp.tests_limit,
        custom_markers: &config.test.markers,
    };

    let analysis = match LocationAnalysis::at(app.lsp.as_ref(), loc.clone()).await {
        Ok(a) => a,
        Err(e) => {
            ctx.print_error(format_analysis_transport_error(
                &e.to_string(),
                &loc.file,
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
        app.test_matcher(),
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
    test_matcher: &TestMatcher,
    params: &ContextParams<'_>,
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
        );
        if let Some(sym) = analysis.target.as_ref() {
            t = t.with_signature(extract_signature(sym.body.as_deref()));
            if args.body || args.all {
                t = t.with_body(sym.body.clone());
            }
        }
        t
    };

    // Honour the per-call references limit by classifying a truncated slice
    // of the analysis output. Keeps the heavy LSP round-trip in
    // `LocationAnalysis::at` while letting commands cap their response size.
    let limit = params.refs;
    let refs_slice: &[crate::models::symbol::Location] = if analysis.references.len() > limit {
        &analysis.references[..limit]
    } else {
        &analysis.references
    };
    let classified = crate::cli::utils::classify_refs(
        refs_slice,
        root,
        Some(&analysis.anchor.file),
        Some(analysis.anchor.line),
        test_matcher,
    );

    let refs_summary = RefOutput {
        total: classified.total,
        test: classified.test,
        prod: classified.prod,
        files: Some(classified.unique_files),
        modules: Some(classified.unique_modules),
        is_exported: analysis.is_exported(),
    };

    let file = analysis.anchor.file.as_path();

    // Fetch optional sections in parallel
    let want_callers = args.callers || args.all;
    let want_callees = args.callees || args.all;
    let want_types = args.types || args.all;
    let want_tests = args.tests || args.all;

    let (callers, callees, types, tests) = tokio::join!(
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
                Some(
                    fetch_tests(
                        &classified.test_refs,
                        root,
                        params.tests,
                        params.custom_markers,
                    )
                    .await,
                )
            } else {
                None
            }
        },
    );

    ContextOutput {
        target,
        refs: refs_summary,
        callers,
        callees,
        types,
        tests,
    }
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
    let result: Result<Vec<CallHierarchyItem>, _> = if incoming {
        lsp.incoming_calls(file, line, column).await
    } else {
        lsp.outgoing_calls(file, line, column).await
    };

    match result {
        Ok(calls) => {
            let total = calls.len();
            let items: Vec<CallHierarchyOutput> = calls
                .iter()
                .take(limit)
                .map(|c| CallHierarchyOutput::from_item(c, root))
                .collect();
            Section::with_total(items, total)
        }
        Err(e) => Section::error(format_call_hierarchy_error(
            &e.to_string(),
            file,
            line,
            column,
        )),
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
                location: LocationOutput::from_path(
                    &type_loc.file,
                    type_loc.line,
                    type_loc.column,
                    root,
                ),
                detail: None,
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

fn is_transport_lsp_failure(error: &str) -> bool {
    error.contains("Broken pipe") || error.contains("timed out") || error.contains("timeout")
}

fn format_analysis_transport_error(
    error: &str,
    file: &Path,
    line: u32,
    column: u32,
) -> OutputError {
    if is_transport_lsp_failure(error) {
        OutputError::lsp_unavailable("The language server did not respond cleanly").with_hint(
            format!(
                "Retry after `symora daemon restart`, or continue with `symora symbols {}` and `symora usage {}:{}:{}`.",
                file.display(),
                file.display(),
                line,
                column,
            ),
        )
    } else {
        OutputError::internal(error.to_string())
    }
}

async fn fetch_tests(
    test_refs: &[&crate::models::symbol::Location],
    root: &Path,
    limit: usize,
    custom_markers: &[String],
) -> Section<TestOutput> {
    let mut items = Vec::with_capacity(limit);

    for r in test_refs.iter().take(limit) {
        if let Ok(content) = tokio::fs::read_to_string(&r.file).await
            && let Some(test_name) = extract_test_name(&content, r.line, custom_markers)
        {
            items.push(TestOutput {
                name: test_name,
                location: LocationOutput::from_path(&r.file, r.line, r.column, root),
            });
        }
    }

    Section::new(items)
}

/// Max lines to search backwards for a test marker annotation
const TEST_MARKER_SEARCH_WINDOW: usize = 10;

fn extract_test_name(content: &str, line: u32, custom_markers: &[String]) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let line_idx = (line.saturating_sub(1)) as usize;

    if line_idx >= lines.len() {
        return None;
    }

    for i in (0..=line_idx.min(TEST_MARKER_SEARCH_WINDOW)).rev() {
        let idx = line_idx.saturating_sub(i);
        let line_content = lines.get(idx)?;

        if is_test_marker(line_content, custom_markers) {
            if let Some(fn_line) = lines.get(idx + 1)
                && let Some(name) = extract_fn_name(fn_line)
            {
                return Some(name);
            }
            if let Some(name) = extract_fn_name(line_content) {
                return Some(name);
            }
        }
    }

    None
}

fn is_test_marker(line: &str, custom_markers: &[String]) -> bool {
    const MARKERS: &[&str] = &[
        "#[test]",
        "#[tokio::test]",
        "#[rstest]",
        "@Test",
        "@ParameterizedTest",
        "[Test]",
        "[Fact]",
        "[Theory]",
        "[TestMethod]",
        "/** @test */",
        "fn test_",
        "func Test",
        "def test_",
        "function test",
        "it(",
        "test(",
        "it \"",
        "it '",
        "it {",
        "should(",
        "test \"",
        "describe(",
        "describe \"",
        "context(",
        "given(",
        "When(",
        "Then(",
    ];

    MARKERS.iter().any(|m| line.contains(m))
        || custom_markers.iter().any(|m| line.contains(m.as_str()))
}

fn extract_fn_name(line: &str) -> Option<String> {
    const FN_PREFIXES: &[&str] = &[
        "fn ",
        "func ",
        "def ",
        "fun ",
        "function ",
        "void ",
        "public void ",
        "async ",
    ];

    for prefix in FN_PREFIXES {
        if let Some(pos) = line.find(prefix) {
            let rest = &line[pos + prefix.len()..];
            let name = rest.split(['(', '<', ' ', ':', '{']).next()?;
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    const STRING_PATTERNS: &[&str] = &[
        "it(",
        "test(",
        "it \"",
        "it '",
        "test \"",
        "describe(",
        "describe \"",
    ];

    for prefix in STRING_PATTERNS {
        if let Some(pos) = line.find(prefix) {
            let rest = &line[pos + prefix.len()..];
            let rest = rest.trim_start_matches(['\'', '"', '(']);
            let name = rest.split(['\'', '"', ',', ')']).next()?;
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    None
}
