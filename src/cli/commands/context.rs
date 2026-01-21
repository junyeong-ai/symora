//! Context command - gather all related code context in a single call

use std::path::Path;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{CallHierarchyOutput, LocationOutput};
use crate::cli::utils::{TestMatcher, extract_signature, find_symbol_at_line};
use crate::models::config::LspConfig;
use crate::models::lsp::{CallHierarchyItem, FindSymbolsOptions};
use crate::services::lsp::LspService;

#[derive(Args, Debug)]
pub struct ContextArgs {
    /// Location (file:line:column)
    pub location: String,

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
}

#[derive(Debug, Serialize)]
pub struct ContextResponse {
    pub target: TargetInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers: Option<ContextSection<CallHierarchyOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callees: Option<ContextSection<CallHierarchyOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub types: Option<ContextSection<TypeInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<ContextSection<TestInfo>>,
    pub references: ContextSection<ReferenceInfo>,
}

#[derive(Debug, Serialize)]
pub struct ContextSection<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T> ContextSection<T> {
    fn success(items: Vec<T>) -> Self {
        Self { items, error: None }
    }

    fn error(msg: impl Into<String>) -> Self {
        Self {
            items: vec![],
            error: Some(msg.into()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TargetInfo {
    pub name: String,
    pub kind: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TypeInfo {
    pub name: String,
    pub kind: String,
    pub location: LocationOutput,
}

#[derive(Debug, Serialize)]
pub struct TestInfo {
    pub name: String,
    pub location: LocationOutput,
}

#[derive(Debug, Serialize)]
pub struct ReferenceInfo {
    pub location: LocationOutput,
    pub is_test: bool,
}

struct ContextLimits {
    calls: usize,
    refs: usize,
    tests: usize,
}

impl From<&LspConfig> for ContextLimits {
    fn from(cfg: &LspConfig) -> Self {
        Self {
            calls: cfg.calls_limit,
            refs: cfg.refs_limit,
            tests: cfg.tests_limit,
        }
    }
}

pub async fn execute(args: ContextArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;
    let limits = ContextLimits::from(&app.config().lsp);

    let response = gather_context(
        app.lsp.as_ref(),
        &loc.file,
        loc.line,
        loc.column,
        &args,
        ctx.root(),
        &app.test_matcher(),
        &limits,
    )
    .await;

    match response {
        Ok(context) => ctx.print_success_flat(context),
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

async fn gather_context(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    args: &ContextArgs,
    root: &Path,
    test_matcher: &TestMatcher,
    limits: &ContextLimits,
) -> Result<ContextResponse> {
    let (refs_result, symbols_result) = tokio::join!(
        lsp.find_references(file, line, column),
        lsp.find_symbols(file, FindSymbolsOptions::new().with_body().with_depth(10))
    );

    let symbols = symbols_result.unwrap_or_default();
    let target_symbol = find_symbol_at_line(&symbols, line);

    let target = TargetInfo {
        name: target_symbol
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "unknown".to_string()),
        kind: target_symbol
            .map(|s| s.kind.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        location: LocationOutput::from_path(file, line, column, root),
        signature: target_symbol.and_then(|s| extract_signature(s.body.as_deref())),
        body: target_symbol.and_then(|s| s.body.clone()),
    };

    let refs = refs_result.unwrap_or_default();

    let refs_with_test_flag: Vec<_> = refs
        .iter()
        .filter(|r| r.file != file || r.line != line)
        .map(|r| (r, test_matcher.is_test_file(&r.file)))
        .collect();

    let test_refs: Vec<_> = refs_with_test_flag
        .iter()
        .filter(|(_, is_test)| *is_test)
        .map(|(r, _)| *r)
        .collect();

    let references = ContextSection::success(
        refs_with_test_flag
            .iter()
            .take(limits.refs)
            .map(|(r, is_test)| ReferenceInfo {
                location: LocationOutput::from_path(&r.file, r.line, r.column, root),
                is_test: *is_test,
            })
            .collect(),
    );

    let callers = if args.callers || args.all {
        Some(fetch_calls(lsp, file, line, column, root, limits.calls, true).await)
    } else {
        None
    };

    let callees = if args.callees || args.all {
        Some(fetch_calls(lsp, file, line, column, root, limits.calls, false).await)
    } else {
        None
    };

    let types = if args.types || args.all {
        Some(fetch_types(lsp, file, line, column, root).await)
    } else {
        None
    };

    let tests = if args.tests || args.all {
        Some(fetch_tests(&test_refs, root, limits.tests).await)
    } else {
        None
    };

    Ok(ContextResponse {
        target,
        callers,
        callees,
        types,
        tests,
        references,
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
) -> ContextSection<CallHierarchyOutput> {
    let result: Result<Vec<CallHierarchyItem>, _> = if incoming {
        lsp.incoming_calls(file, line, column).await
    } else {
        lsp.outgoing_calls(file, line, column).await
    };

    match result {
        Ok(calls) => {
            let items = calls
                .iter()
                .take(limit)
                .map(|c| CallHierarchyOutput::from_item(c, root))
                .collect();
            ContextSection::success(items)
        }
        Err(e) => ContextSection::error(e.to_string()),
    }
}

async fn fetch_types(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    root: &Path,
) -> ContextSection<TypeInfo> {
    match lsp.goto_type_definition(file, line, column).await {
        Ok(Some(type_loc)) => {
            let type_symbols = lsp
                .find_symbols(&type_loc.file, FindSymbolsOptions::default())
                .await
                .unwrap_or_default();

            let type_sym = find_symbol_at_line(&type_symbols, type_loc.line);
            let items = vec![TypeInfo {
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
            }];
            ContextSection::success(items)
        }
        Ok(None) => ContextSection::success(vec![]),
        Err(e) => ContextSection::error(e.to_string()),
    }
}

async fn fetch_tests(
    test_refs: &[&crate::models::symbol::Location],
    root: &Path,
    limit: usize,
) -> ContextSection<TestInfo> {
    let mut items = Vec::with_capacity(limit);

    for r in test_refs.iter().take(limit) {
        if let Ok(content) = tokio::fs::read_to_string(&r.file).await {
            if let Some(test_name) = extract_test_name(&content, r.line) {
                items.push(TestInfo {
                    name: test_name,
                    location: LocationOutput::from_path(&r.file, r.line, r.column, root),
                });
            }
        }
    }

    ContextSection::success(items)
}

fn extract_test_name(content: &str, line: u32) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let line_idx = (line.saturating_sub(1)) as usize;

    if line_idx >= lines.len() {
        return None;
    }

    for i in (0..=line_idx.min(10)).rev() {
        let idx = line_idx.saturating_sub(i);
        let line_content = lines.get(idx)?;

        if is_test_marker(line_content) {
            if let Some(fn_line) = lines.get(idx + 1) {
                if let Some(name) = extract_fn_name(fn_line) {
                    return Some(name);
                }
            }
            if let Some(name) = extract_fn_name(line_content) {
                return Some(name);
            }
        }
    }

    None
}

fn is_test_marker(line: &str) -> bool {
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
}

fn extract_fn_name(line: &str) -> Option<String> {
    const FN_PATTERNS: &[(&str, usize)] = &[
        ("fn ", 3),
        ("func ", 5),
        ("def ", 4),
        ("fun ", 4),
        ("function ", 9),
        ("void ", 5),
        ("public void ", 12),
        ("async ", 6),
    ];

    for &(prefix, offset) in FN_PATTERNS {
        if let Some(pos) = line.find(prefix) {
            let rest = &line[pos + offset..];
            let name = rest.split(['(', '<', ' ', ':', '{']).next()?;
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    const STRING_PATTERNS: &[&str] = &[
        "it(", "test(", "it \"", "it '", "test \"", "describe(", "describe \"",
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
