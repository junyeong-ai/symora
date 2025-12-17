//! Diagnostics command implementation

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::response::{DiagnosticOutput, DiagnosticsResponse};
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
}

pub async fn execute(args: DiagnosticsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    let abs_file = if args.file.is_absolute() {
        args.file.clone()
    } else {
        app.root().join(&args.file)
    };

    // Parse severity filters
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

            let response = DiagnosticsResponse {
                file: ctx.relative_path(&args.file),
                count: filtered.len(),
                diagnostics: filtered
                    .iter()
                    .map(|d| DiagnosticOutput {
                        severity: d.severity.to_string(),
                        message: d.message.clone(),
                        line: d.display_line(),
                        column: d.display_column(),
                        end_line: d.display_end_line(),
                        end_column: d.display_end_column(),
                        code: d.code.clone(),
                        source: d.source.clone(),
                        tags: d.tags.iter().map(|t| t.to_string()).collect(),
                    })
                    .collect(),
            };
            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
