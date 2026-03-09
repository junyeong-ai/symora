use std::path::Path;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::{
    CallHierarchyOutput, LocationOutput, RefOutput, Section, TargetOutput, TestOutput,
    TypeInfoOutput,
};
use crate::cli::utils::{
    TestMatcher, classify_refs, extract_signature, find_symbol_at_position, resolve_symbol_anchor,
};
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

    let response = fetch_context(
        app.lsp.as_ref(),
        &loc.file,
        loc.line,
        loc.column,
        &args,
        ctx.root(),
        app.test_matcher(),
        &params,
    )
    .await;

    match response {
        Ok(context) => ctx.print_success(context),
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

async fn fetch_context(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    args: &ContextArgs,
    root: &Path,
    test_matcher: &TestMatcher,
    params: &ContextParams<'_>,
) -> Result<ContextOutput> {
    let (refs_result, symbols_result) = tokio::join!(
        lsp.find_references(file, line, column),
        lsp.find_symbols(
            file,
            FindSymbolsOptions::default().with_body().with_depth(10)
        )
    );

    let symbols = symbols_result.unwrap_or_default();
    let resolved = resolve_symbol_anchor(&symbols, line, column);
    let resolved_line = resolved.map(|(line, _, _)| line).unwrap_or(line);
    let resolved_column = resolved.map(|(_, column, _)| column).unwrap_or(column);
    let target_symbol = resolved.map(|(_, _, symbol)| symbol);

    let target = {
        let mut t = TargetOutput::from_symbol_or_fallback(
            target_symbol,
            file,
            resolved_line,
            resolved_column,
            root,
        );
        if let Some(sym) = target_symbol {
            t = t.with_signature(extract_signature(sym.body.as_deref()));
            if args.body || args.all {
                t = t.with_body(sym.body.clone());
            }
        }
        t
    };

    let refs_result = if target_symbol.is_none() && refs_result.is_err() {
        lsp.find_references(file, resolved_line, resolved_column)
            .await
    } else {
        refs_result
    };

    let mut refs = refs_result.unwrap_or_default();
    refs.truncate(params.refs);
    let classified = classify_refs(&refs, root, Some(file), Some(line), test_matcher);

    let refs_summary = RefOutput {
        total: classified.total,
        test: classified.test,
        prod: classified.prod,
        files: Some(classified.unique_files),
        modules: Some(classified.unique_modules),
        is_exported: None,
    };

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

    Ok(ContextOutput {
        target,
        refs: refs_summary,
        callers,
        callees,
        types,
        tests,
    })
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
            Section::with_limit(items, total)
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

fn format_call_hierarchy_error(error: &str, file: &Path, line: u32, column: u32) -> String {
    if is_unsupported_lsp_feature(error) {
        format!(
            "Call hierarchy is not supported here. Use `symora refs {}:{}:{}` for usages or `symora usage {}:{}:{}` for broader symbol analysis.",
            file.display(),
            line,
            column,
            file.display(),
            line,
            column
        )
    } else {
        error.to_string()
    }
}

fn format_type_error(error: &str, file: &Path, line: u32, column: u32) -> String {
    if is_unsupported_lsp_feature(error) {
        format!(
            "Type definition lookup is not supported here. Use `symora hover {}:{}:{}` or inspect the target body in this context output instead.",
            file.display(),
            line,
            column
        )
    } else {
        error.to_string()
    }
}

fn is_unsupported_lsp_feature(error: &str) -> bool {
    error.contains("does not support") || error.contains("no handler for request")
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
