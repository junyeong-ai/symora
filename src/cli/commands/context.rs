//! Context command - gather all related code context in a single call

use std::path::Path;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{CallHierarchyOutput, LocationOutput};
use crate::cli::utils::{TestMatcher, extract_signature, find_symbol_at_line};
use crate::models::lsp::FindSymbolsOptions;
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<CallHierarchyOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callees: Vec<CallHierarchyOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<TypeInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ReferenceInfo>,
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

pub async fn execute(args: ContextArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;
    let test_matcher = app.test_matcher();

    let response = gather_context(
        app.lsp.as_ref(),
        &loc.file,
        loc.line,
        loc.column,
        &args,
        ctx.root(),
        &test_matcher,
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

    let references: Vec<ReferenceInfo> = refs_with_test_flag
        .iter()
        .take(20)
        .map(|(r, is_test)| ReferenceInfo {
            location: LocationOutput::from_path(&r.file, r.line, r.column, root),
            is_test: *is_test,
        })
        .collect();

    let callers = if args.callers || args.all {
        lsp.incoming_calls(file, line, column)
            .await
            .map(|calls| {
                calls
                    .iter()
                    .take(10)
                    .map(|c| CallHierarchyOutput::from_item(c, root))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let callees = if args.callees || args.all {
        lsp.outgoing_calls(file, line, column)
            .await
            .map(|calls| {
                calls
                    .iter()
                    .take(10)
                    .map(|c| CallHierarchyOutput::from_item(c, root))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let types = if args.types || args.all {
        match lsp.goto_type_definition(file, line, column).await {
            Ok(Some(type_loc)) => {
                if let Ok(type_symbols) = lsp
                    .find_symbols(&type_loc.file, FindSymbolsOptions::default())
                    .await
                {
                    let type_sym = find_symbol_at_line(&type_symbols, type_loc.line);
                    vec![TypeInfo {
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
                    }]
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    } else {
        vec![]
    };

    let tests = if args.tests || args.all {
        let mut tests = Vec::with_capacity(5);
        for r in test_refs.iter().take(5) {
            if let Ok(content) = tokio::fs::read_to_string(&r.file).await
                && let Some(test_name) = extract_test_name(&content, r.line)
            {
                tests.push(TestInfo {
                    name: test_name,
                    location: LocationOutput::from_path(&r.file, r.line, r.column, root),
                });
            }
        }
        tests
    } else {
        vec![]
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

fn extract_test_name(content: &str, line: u32) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let line_idx = (line.saturating_sub(1)) as usize;

    if line_idx >= lines.len() {
        return None;
    }

    for i in (0..=line_idx.min(10)).rev() {
        let idx = line_idx.saturating_sub(i);
        let line_content = lines.get(idx)?;

        // Test markers by language
        let is_test_marker = line_content.contains("#[test]")           // Rust
            || line_content.contains("#[tokio::test]")                  // Rust async
            || line_content.contains("#[rstest]")                       // Rust rstest
            || line_content.contains("@Test")                           // Java, Kotlin JUnit
            || line_content.contains("@ParameterizedTest")              // JUnit 5
            || line_content.contains("[Test]")                          // C# NUnit
            || line_content.contains("[Fact]")                          // C# xUnit
            || line_content.contains("[Theory]")                        // C# xUnit
            || line_content.contains("[TestMethod]")                    // C# MSTest
            || line_content.contains("/** @test */")                    // PHP
            || line_content.contains("fn test_")                        // Rust
            || line_content.contains("func Test")                       // Go, Swift
            || line_content.contains("def test_")                       // Python
            || line_content.contains("function test")                   // PHP
            || line_content.contains("it(")                             // JS/TS Mocha/Jest
            || line_content.contains("test(")                           // JS/TS Jest, Dart, Elixir
            || line_content.contains("it \"")                           // Ruby RSpec
            || line_content.contains("it '")                            // Ruby RSpec
            || line_content.contains("it {")                            // Kotest
            || line_content.contains("should(")                         // Kotest
            || line_content.contains("test \"")                         // Elixir
            || line_content.contains("describe(")                       // JS/TS, Kotest
            || line_content.contains("describe \"")                     // Ruby
            || line_content.contains("context(")                        // Kotest, RSpec
            || line_content.contains("given(")                          // Kotest BehaviorSpec
            || line_content.contains("When(")                           // Kotest BehaviorSpec
            || line_content.contains("Then("); // Kotest BehaviorSpec

        if is_test_marker {
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

fn extract_fn_name(line: &str) -> Option<String> {
    // Function declaration patterns by language
    for (prefix, offset) in [
        ("fn ", 3),           // Rust
        ("func ", 5),         // Go, Swift
        ("def ", 4),          // Python, Ruby, Elixir
        ("fun ", 4),          // Kotlin
        ("function ", 9),     // PHP, JS
        ("void ", 5),         // Java, C#
        ("public void ", 12), // Java, C#
        ("async ", 6),        // JS/TS async
    ] {
        if let Some(pos) = line.find(prefix) {
            let rest = &line[pos + offset..];
            let name = rest.split(['(', '<', ' ', ':', '{']).next()?;
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    // String-based test names (JS/TS, Ruby, Elixir)
    for prefix in [
        "it(",
        "test(",
        "it \"",
        "it '",
        "test \"",
        "describe(",
        "describe \"",
    ] {
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
