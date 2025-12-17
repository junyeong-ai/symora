//! Batch command implementation
//!
//! Execute multiple commands in a single request for efficiency.
//! Commands are read from stdin in JSON format.

use std::io::{self, BufRead};

use anyhow::Result;
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::models::lsp::FindSymbolsOptions;

#[derive(Args, Debug)]
pub struct BatchArgs {
    /// Execute commands in parallel when possible
    #[arg(long)]
    pub parallel: bool,

    /// Stop on first error
    #[arg(long)]
    pub fail_fast: bool,
}

/// A single batch command
#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum BatchCommand {
    FindSymbol { file: String },
    FindRefs { location: String },
    FindDef { location: String },
    FindTypedef { location: String },
    FindImpl { location: String },
    Hover { location: String },
    Diagnostics { file: String },
    Rename { location: String, new_name: String },
    CallsIncoming { location: String },
    CallsOutgoing { location: String },
}

/// Result of a batch command execution
#[derive(Debug, Serialize)]
struct BatchResult {
    /// Index of the command in the batch
    index: usize,
    /// Whether the command succeeded
    success: bool,
    /// Result data (if success)
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    /// Error message (if failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Full batch response
#[derive(Debug, Serialize)]
struct BatchResponse {
    /// Total commands processed
    total: usize,
    /// Number of successes
    successes: usize,
    /// Number of failures
    failures: usize,
    /// Individual results
    results: Vec<BatchResult>,
}

pub async fn execute(args: BatchArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    // Read commands from stdin
    let stdin = io::stdin();
    let commands: Vec<BatchCommand> = stdin
        .lock()
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect();

    if commands.is_empty() {
        ctx.print_success_flat(serde_json::json!({
            "total": 0,
            "message": "No commands provided. Send JSON commands via stdin, one per line."
        }));
        return Ok(());
    }

    let mut results = Vec::with_capacity(commands.len());

    if args.parallel {
        // Execute commands in parallel using tokio::join_all
        let futures: Vec<_> = commands
            .iter()
            .enumerate()
            .map(|(i, cmd)| async move {
                let result = execute_single_command(cmd, app).await;
                (i, result)
            })
            .collect();

        let parallel_results = futures::future::join_all(futures).await;

        for (index, result) in parallel_results {
            match result {
                Ok(data) => {
                    results.push(BatchResult {
                        index,
                        success: true,
                        result: Some(data),
                        error: None,
                    });
                }
                Err(e) => {
                    results.push(BatchResult {
                        index,
                        success: false,
                        result: None,
                        error: Some(e.to_string()),
                    });
                    if args.fail_fast {
                        break;
                    }
                }
            }
        }
    } else {
        // Execute commands sequentially
        for (index, cmd) in commands.iter().enumerate() {
            match execute_single_command(cmd, app).await {
                Ok(data) => {
                    results.push(BatchResult {
                        index,
                        success: true,
                        result: Some(data),
                        error: None,
                    });
                }
                Err(e) => {
                    results.push(BatchResult {
                        index,
                        success: false,
                        result: None,
                        error: Some(e.to_string()),
                    });
                    if args.fail_fast {
                        break;
                    }
                }
            }
        }
    }

    let successes = results.iter().filter(|r| r.success).count();
    let failures = results.len() - successes;

    let response = BatchResponse {
        total: results.len(),
        successes,
        failures,
        results,
    };

    ctx.print_success_flat(response);

    Ok(())
}

/// Execute a single batch command
async fn execute_single_command(
    cmd: &BatchCommand,
    app: &App,
) -> Result<serde_json::Value, anyhow::Error> {
    let ctx = &app.output;

    match cmd {
        BatchCommand::FindSymbol { file } => {
            let path = std::env::current_dir()?.join(file);
            let symbols = app
                .lsp
                .find_symbols(&path, FindSymbolsOptions::default())
                .await?;

            Ok(serde_json::json!({
                "count": symbols.len(),
                "symbols": symbols.iter().map(|s| serde_json::json!({
                    "name": s.name,
                    "kind": s.kind.to_string(),
                    "file": s.location.file.display().to_string(),
                    "line": s.location.line,
                    "column": s.location.column,
                })).collect::<Vec<_>>()
            }))
        }

        BatchCommand::FindRefs { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let refs = app
                .lsp
                .find_references(&loc.file, loc.line, loc.column)
                .await?;

            // Filter to project-only references
            let project_refs: Vec<_> = refs
                .iter()
                .filter(|l| ctx.is_project_path(&l.file))
                .collect();

            Ok(serde_json::json!({
                "count": project_refs.len(),
                "references": project_refs.iter().map(|r| serde_json::json!({
                    "file": r.file.display().to_string(),
                    "line": r.line,
                    "column": r.column,
                })).collect::<Vec<_>>()
            }))
        }

        BatchCommand::FindDef { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let def = app
                .lsp
                .goto_definition(&loc.file, loc.line, loc.column)
                .await?;

            match def {
                Some(d) => Ok(serde_json::json!({
                    "definition": {
                        "file": d.file.display().to_string(),
                        "line": d.line,
                        "column": d.column,
                    }
                })),
                None => Ok(serde_json::json!({
                    "definition": null,
                    "message": "No definition found"
                })),
            }
        }

        BatchCommand::FindTypedef { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let def = app
                .lsp
                .goto_type_definition(&loc.file, loc.line, loc.column)
                .await?;

            match def {
                Some(d) => Ok(serde_json::json!({
                    "definition": {
                        "file": d.file.display().to_string(),
                        "line": d.line,
                        "column": d.column,
                    }
                })),
                None => Ok(serde_json::json!({
                    "definition": null,
                    "message": "No type definition found"
                })),
            }
        }

        BatchCommand::FindImpl { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let impls = app
                .lsp
                .find_implementations(&loc.file, loc.line, loc.column)
                .await?;

            Ok(serde_json::json!({
                "count": impls.len(),
                "implementations": impls.iter().map(|i| serde_json::json!({
                    "file": i.file.display().to_string(),
                    "line": i.line,
                    "column": i.column,
                })).collect::<Vec<_>>()
            }))
        }

        BatchCommand::Hover { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let hover = app.lsp.hover(&loc.file, loc.line, loc.column).await?;

            match hover {
                Some(h) => Ok(serde_json::json!({
                    "content": h.content,
                })),
                None => Ok(serde_json::json!({
                    "content": null,
                    "message": "No hover information"
                })),
            }
        }

        BatchCommand::Diagnostics { file } => {
            let path = std::env::current_dir()?.join(file);
            let diags = app.lsp.diagnostics(&path).await?;

            Ok(serde_json::json!({
                "count": diags.len(),
                "diagnostics": diags.iter().map(|d| serde_json::json!({
                    "message": d.message,
                    "severity": format!("{:?}", d.severity),
                    "line": d.range.start.line + 1,
                    "column": d.range.start.character + 1,
                })).collect::<Vec<_>>()
            }))
        }

        BatchCommand::Rename { location, new_name } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let result = app
                .lsp
                .rename(&loc.file, loc.line, loc.column, new_name)
                .await?;

            Ok(serde_json::json!({
                "changes": result.changes.iter().map(|c| serde_json::json!({
                    "file": c.file.display().to_string(),
                    "edit_count": c.edit_count,
                })).collect::<Vec<_>>()
            }))
        }

        BatchCommand::CallsIncoming { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let calls = app
                .lsp
                .incoming_calls(&loc.file, loc.line, loc.column)
                .await?;

            Ok(serde_json::json!({
                "count": calls.len(),
                "calls": calls.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "file": c.location.file.display().to_string(),
                    "line": c.location.line,
                    "column": c.location.column,
                })).collect::<Vec<_>>()
            }))
        }

        BatchCommand::CallsOutgoing { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let calls = app
                .lsp
                .outgoing_calls(&loc.file, loc.line, loc.column)
                .await?;

            Ok(serde_json::json!({
                "count": calls.len(),
                "calls": calls.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "file": c.location.file.display().to_string(),
                    "line": c.location.line,
                    "column": c.location.column,
                })).collect::<Vec<_>>()
            }))
        }
    }
}
