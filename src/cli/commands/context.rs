//! Context command - gather all related code context in a single call

use std::path::Path;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::LocationOutput;
use crate::cli::utils::{extract_signature, find_symbol_at_line, is_test_file};
use crate::models::lsp::FindSymbolsOptions;
use crate::services::lsp::LspService;

#[derive(Args, Debug)]
pub struct ContextArgs {
    /// Location (file:line:column)
    pub location: String,

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
    pub callers: Vec<CallerInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callees: Vec<CalleeInfo>,
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
pub struct CallerInfo {
    pub name: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<LocationOutput>,
}

#[derive(Debug, Serialize)]
pub struct CalleeInfo {
    pub name: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<LocationOutput>,
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

    let response = gather_context(
        app.lsp.as_ref(),
        &loc.file,
        loc.line,
        loc.column,
        &args,
        ctx.root(),
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
        .map(|r| (r, is_test_file(&r.file)))
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

    let callers = if args.callers {
        lsp.incoming_calls(file, line, column)
            .await
            .map(|calls| {
                calls
                    .into_iter()
                    .take(10)
                    .map(|c| CallerInfo {
                        name: c.name,
                        location: LocationOutput::from_path(
                            &c.location.file,
                            c.location.line,
                            c.location.column,
                            root,
                        ),
                        call_site: c.call_site.map(|cs| {
                            LocationOutput::from_path(&cs.file, cs.line, cs.column, root)
                        }),
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let callees = if args.callees {
        lsp.outgoing_calls(file, line, column)
            .await
            .map(|calls| {
                calls
                    .into_iter()
                    .take(10)
                    .map(|c| CalleeInfo {
                        name: c.name,
                        location: LocationOutput::from_path(
                            &c.location.file,
                            c.location.line,
                            c.location.column,
                            root,
                        ),
                        call_site: c.call_site.map(|cs| {
                            LocationOutput::from_path(&cs.file, cs.line, cs.column, root)
                        }),
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let types = if args.types {
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

    let tests = if args.tests {
        test_refs
            .iter()
            .take(5)
            .filter_map(|r| {
                let content = std::fs::read_to_string(&r.file).ok()?;
                let test_name = extract_test_name(&content, r.line)?;
                Some(TestInfo {
                    name: test_name,
                    location: LocationOutput::from_path(&r.file, r.line, r.column, root),
                })
            })
            .collect()
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

        if line_content.contains("#[test]")
            || line_content.contains("fn test_")
            || line_content.contains("func Test")
            || line_content.contains("def test_")
            || line_content.contains("it(")
            || line_content.contains("test(")
        {
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
    for (prefix, offset) in [("fn ", 3), ("func ", 5), ("def ", 4)] {
        if let Some(pos) = line.find(prefix) {
            let rest = &line[pos + offset..];
            let name = rest.split(['(', '<', ' ', ':']).next()?;
            return Some(name.to_string());
        }
    }

    for prefix in ["it(", "test("] {
        if let Some(pos) = line.find(prefix) {
            let rest = &line[pos + prefix.len()..];
            let name = rest.trim_start_matches(['\'', '"']);
            let name = name.split(['\'', '"']).next()?;
            return Some(name.to_string());
        }
    }

    None
}
