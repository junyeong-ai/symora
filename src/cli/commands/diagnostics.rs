use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::{DiagnosticOutput, Section};
use crate::models::diagnostic::{DiagnosticSeverity, DiagnosticsStatus};

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
    /// Present only when the result is *not* authoritative:
    /// `unconfirmed` (server never confirmed analyzing this content) or
    /// `unsupported` (the language's server doesn't publish diagnostics).
    /// Absent means the server confirmed the listed diagnostics.
    #[serde(skip_serializing_if = "DiagnosticsStatus::is_ok")]
    pub status: DiagnosticsStatus,
    /// The diagnostics list, flattened in as the shared list contract.
    #[serde(flatten)]
    pub diagnostics: Section<EnhancedDiagnostic>,
}

#[derive(Debug, Serialize)]
pub struct EnhancedDiagnostic {
    #[serde(flatten)]
    pub base: DiagnosticOutput,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<DiagnosticContextItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<DiagnosticSuggestion>,
    /// Present only when suggestions were requested but the quickfix lookup
    /// failed ("unavailable") — an empty `suggestions` then means "unknown",
    /// never an authoritative "no fixes available".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestions_status: Option<&'static str>,
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

/// Parse and validate the raw severity filter. Unknown values are
/// rejected rather than dropped: a silently dropped term can filter
/// every diagnostic out and make a broken file look clean. Empty
/// segments (a trailing comma) are ignored; a filter made of only empty
/// segments is rejected as a caller error rather than read as "no
/// filter".
fn parse_severity_filter(
    raw: Option<&[String]>,
) -> Result<Option<Vec<DiagnosticSeverity>>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let segments: Vec<&str> = raw
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return Err(format!(
            "Unknown severity: '{}'. Valid: error, warning, info, hint",
            raw.join(",")
        ));
    }
    segments
        .into_iter()
        .map(str::parse::<DiagnosticSeverity>)
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

pub async fn execute(args: DiagnosticsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    let abs_file = if args.file.is_absolute() {
        args.file.clone()
    } else {
        app.root().join(&args.file)
    };

    let severity_filter = match parse_severity_filter(args.severity.as_deref()) {
        Ok(filter) => filter,
        Err(message) => {
            ctx.print_error(OutputError::invalid(message));
            return Ok(());
        }
    };

    match app.lsp.diagnostics(&abs_file).await {
        Ok(report) => {
            let status = report.status;
            let filtered: Vec<_> = report
                .items
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
                let base = DiagnosticOutput::from(d);

                let (context, (suggestions, suggestions_status)) = tokio::join!(
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
                            (vec![], None)
                        }
                    },
                );

                enhanced_diagnostics.push(EnhancedDiagnostic {
                    base,
                    context,
                    suggestions,
                    suggestions_status,
                });
            }

            let response = DiagnosticsOutput {
                file: ctx.relative_path(&abs_file),
                status,
                diagnostics: Section::new(enhanced_diagnostics),
            };
            ctx.print_success(response);
        }
        Err(e) => ctx.print_error(e),
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
) -> (Vec<DiagnosticSuggestion>, Option<&'static str>) {
    let actions = match app.lsp.code_actions(file, line, column).await {
        Ok(a) => a,
        // The quickfix lookup failed — disclose it so an empty suggestion list
        // reads as "unknown", never an authoritative "no fixes available".
        Err(_) => return (vec![], Some("unavailable")),
    };

    let suggestions = actions
        .into_iter()
        .filter(|a| a.kind.to_string().contains("quickfix"))
        .take(3)
        .map(|a| DiagnosticSuggestion {
            title: a.title,
            code: None,
        })
        .collect();
    (suggestions, None)
}

fn extract_snippet(content: &str, line: u32) -> String {
    let idx = (line.saturating_sub(1)) as usize;
    content
        .lines()
        .nth(idx)
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(status: DiagnosticsStatus) -> serde_json::Value {
        serde_json::to_value(DiagnosticsOutput {
            file: "src/lib.rs".into(),
            status,
            diagnostics: Section::new(vec![]),
        })
        .unwrap()
    }

    /// `status` appears exactly when the result is not authoritative —
    /// an agent that sees no `status` key may trust the list as-is.
    #[test]
    fn status_is_omitted_only_when_confirmed() {
        assert!(output(DiagnosticsStatus::Ok).get("status").is_none());
        assert_eq!(
            output(DiagnosticsStatus::Unconfirmed)["status"],
            "unconfirmed"
        );
        assert_eq!(
            output(DiagnosticsStatus::Unsupported)["status"],
            "unsupported"
        );
    }

    #[test]
    fn diagnostics_output_flattens_the_section_contract() {
        let value = output(DiagnosticsStatus::Ok);
        assert_eq!(value["file"], "src/lib.rs");
        assert_eq!(value["count"], 0);
        assert_eq!(value["showing"], 0);
        assert!(value["items"].is_array());
        assert!(
            value.get("diagnostics").is_none(),
            "section must be flattened, not nested under `diagnostics`"
        );
    }

    #[test]
    fn severity_filter_rejects_unknown_values() {
        let err = parse_severity_filter(Some(&["error".into(), "bogus".into()])).unwrap_err();
        assert!(err.contains("bogus"));
    }

    #[test]
    fn severity_filter_parses_aliases_and_trims() {
        let parsed = parse_severity_filter(Some(&[" warn ".into(), "E".into()]))
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed,
            vec![DiagnosticSeverity::Warning, DiagnosticSeverity::Error]
        );
    }

    /// A trailing comma ("error,") splits into an empty segment — ignored,
    /// not parsed into an unknown-severity error.
    #[test]
    fn severity_filter_ignores_empty_segments() {
        let parsed = parse_severity_filter(Some(&["error".into(), "".into()]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed, vec![DiagnosticSeverity::Error]);
    }

    /// An explicit filter of only empty segments is a caller error, not an
    /// absent filter — silently widening to everything would mask the typo.
    #[test]
    fn severity_filter_rejects_only_empty_segments() {
        let err = parse_severity_filter(Some(&["".into(), " ".into()])).unwrap_err();
        assert!(err.contains("Unknown severity"));
        assert!(err.contains("Valid: error, warning, info, hint"));
    }
}
