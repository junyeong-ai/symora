use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::DiagnosticOutput;
use crate::models::diagnostic::DiagnosticSeverity;

#[derive(Args, Debug)]
pub struct DiagnosticsArgs {
    /// File path to check
    pub file: PathBuf,

    /// Filter by severity (error, warning, info, hint)
    #[arg(long, short = 's', value_delimiter = ',')]
    pub severity: Option<Vec<String>>,

    /// Filter by source (e.g., rust-analyzer, eslint)
    #[arg(long)]
    pub source: Option<String>,

    /// Include related code context
    #[arg(long)]
    pub with_context: bool,

    /// Include fix suggestions from LSP
    #[arg(long)]
    pub with_suggestions: bool,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticsOutput {
    pub file: String,
    pub count: usize,
    pub diagnostics: Vec<EnhancedDiagnostic>,
}

#[derive(Debug, Serialize)]
pub struct EnhancedDiagnostic {
    #[serde(flatten)]
    pub base: DiagnosticOutput,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<DiagnosticContextItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<DiagnosticSuggestion>,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticContextItem {
    pub file: String,
    pub line: u32,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticSuggestion {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

pub async fn execute(args: DiagnosticsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    let abs_file = if args.file.is_absolute() {
        args.file.clone()
    } else {
        app.root().join(&args.file)
    };

    let severity_filter: Option<Vec<DiagnosticSeverity>> = args.severity.as_ref().map(|sevs| {
        sevs.iter()
            .filter_map(|s| s.parse::<DiagnosticSeverity>().ok())
            .collect()
    });

    match app.lsp.diagnostics(&abs_file).await {
        Ok(diagnostics) => {
            let filtered: Vec<_> = diagnostics
                .into_iter()
                .filter(|d| {
                    if let Some(ref filter) = severity_filter
                        && !filter.contains(&d.severity)
                    {
                        return false;
                    }
                    if let Some(ref source) = args.source
                        && d.source.as_ref() != Some(source)
                    {
                        return false;
                    }
                    true
                })
                .collect();

            let mut enhanced_diagnostics = Vec::with_capacity(filtered.len());

            for d in &filtered {
                let base = DiagnosticOutput {
                    severity: d.severity.to_string(),
                    message: d.message.clone(),
                    line: d.display_line(),
                    column: d.display_column(),
                    end_line: d.display_end_line(),
                    end_column: d.display_end_column(),
                    code: d.code.clone(),
                    source: d.source.clone(),
                    tags: d.tags.iter().map(|t| t.to_string()).collect(),
                };

                let (context, suggestions) = tokio::join!(
                    async {
                        if args.with_context {
                            fetch_diagnostic_context(
                                app,
                                &abs_file,
                                d.display_line(),
                                d.display_column(),
                                ctx.root(),
                            )
                            .await
                        } else {
                            vec![]
                        }
                    },
                    async {
                        if args.with_suggestions {
                            fetch_suggestions(app, &abs_file, d.display_line(), d.display_column())
                                .await
                        } else {
                            vec![]
                        }
                    },
                );

                enhanced_diagnostics.push(EnhancedDiagnostic {
                    base,
                    context,
                    suggestions,
                });
            }

            let response = DiagnosticsOutput {
                file: ctx.relative_path(&abs_file),
                count: enhanced_diagnostics.len(),
                diagnostics: enhanced_diagnostics,
            };
            ctx.print_success(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

async fn fetch_diagnostic_context(
    app: &App,
    file: &std::path::Path,
    line: u32,
    column: u32,
    root: &std::path::Path,
) -> Vec<DiagnosticContextItem> {
    let (def_result, type_def_result) = tokio::join!(
        app.lsp.goto_definition(file, line, column),
        app.lsp.goto_type_definition(file, line, column),
    );

    let mut context = Vec::new();
    let mut seen_locations: Vec<(std::path::PathBuf, u32)> = Vec::new();

    if let Ok(Some(def)) = def_result
        && (def.file != file || def.line != line)
        && let Ok(content) = tokio::fs::read_to_string(&def.file).await
    {
        seen_locations.push((def.file.clone(), def.line));
        let snippet = extract_snippet(&content, def.line);
        context.push(DiagnosticContextItem {
            file: def
                .file
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| def.file.display().to_string()),
            line: def.line,
            snippet,
            reason: Some("definition".to_string()),
        });
    }

    if let Ok(Some(type_def)) = type_def_result
        && let Ok(content) = tokio::fs::read_to_string(&type_def.file).await
    {
        let is_duplicate = seen_locations
            .iter()
            .any(|(f, l)| f == &type_def.file && *l == type_def.line);

        if !is_duplicate {
            let snippet = extract_snippet(&content, type_def.line);
            context.push(DiagnosticContextItem {
                file: type_def
                    .file
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| type_def.file.display().to_string()),
                line: type_def.line,
                snippet,
                reason: Some("type_definition".to_string()),
            });
        }
    }

    context
}

async fn fetch_suggestions(
    app: &App,
    file: &std::path::Path,
    line: u32,
    column: u32,
) -> Vec<DiagnosticSuggestion> {
    let actions = match app.lsp.code_actions(file, line, column).await {
        Ok(a) => a,
        Err(_) => return vec![],
    };

    actions
        .into_iter()
        .filter(|a| a.kind.to_string().contains("quickfix"))
        .take(3)
        .map(|a| DiagnosticSuggestion {
            title: a.title,
            code: None,
        })
        .collect()
}

fn extract_snippet(content: &str, line: u32) -> String {
    let idx = (line.saturating_sub(1)) as usize;
    content
        .lines()
        .nth(idx)
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}
