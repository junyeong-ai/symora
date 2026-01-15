//! Batch command implementation
//!
//! Execute multiple commands in a single request for efficiency.
//! Supports both CLI arguments and stdin JSON input.

use std::io::{self, BufRead};
use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Symbol;

#[derive(Args, Debug)]
pub struct BatchArgs {
    #[command(subcommand)]
    pub command: Option<BatchSubcommand>,

    /// Execute commands in parallel when possible
    #[arg(long, global = true)]
    pub parallel: bool,

    /// Stop on first error
    #[arg(long, global = true)]
    pub fail_fast: bool,
}

#[derive(Subcommand, Debug)]
pub enum BatchSubcommand {
    /// Batch hover requests for multiple locations
    Hover {
        /// Locations (file:line:column)
        locations: Vec<String>,
    },

    /// Batch reference lookups for multiple locations
    Refs {
        /// Locations (file:line:column)
        #[arg(required_unless_present = "symbols")]
        locations: Option<Vec<String>>,

        /// Symbol pattern to find refs for all matching symbols
        #[arg(long, short = 's', requires = "file")]
        symbols: Option<String>,

        /// File path (required with --symbols)
        #[arg(long, short)]
        file: Option<String>,
    },

    /// Batch definition lookups for multiple locations
    Def {
        /// Locations (file:line:column)
        locations: Vec<String>,
    },

    /// Batch incoming calls for multiple locations
    Callers {
        /// Locations (file:line:column)
        locations: Vec<String>,
    },

    /// Batch outgoing calls for multiple locations
    Callees {
        /// Locations (file:line:column)
        locations: Vec<String>,
    },

    /// Read commands from stdin (JSON format, one per line)
    Stdin,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum StdinCommand {
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
    Context { location: String },
    Expand { location: String },
}

#[derive(Debug, Serialize)]
struct BatchResult {
    index: usize,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BatchResponse {
    total: usize,
    successes: usize,
    failures: usize,
    results: Vec<BatchResult>,
}

enum BatchOp {
    Hover,
    Refs,
    Def,
    Callers,
    Callees,
}

pub async fn execute(args: BatchArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    match args.command {
        Some(BatchSubcommand::Hover { locations }) => {
            let results = execute_batch(locations, BatchOp::Hover, args.parallel, app).await;
            ctx.print_success_flat(results);
        }

        Some(BatchSubcommand::Refs {
            locations,
            symbols,
            file,
        }) => {
            if let Some(pattern) = symbols {
                let results =
                    execute_batch_by_symbols(&file.unwrap(), &pattern, args.parallel, app).await?;
                ctx.print_success_flat(results);
            } else {
                let results = execute_batch(
                    locations.unwrap_or_default(),
                    BatchOp::Refs,
                    args.parallel,
                    app,
                )
                .await;
                ctx.print_success_flat(results);
            }
        }

        Some(BatchSubcommand::Def { locations }) => {
            let results = execute_batch(locations, BatchOp::Def, args.parallel, app).await;
            ctx.print_success_flat(results);
        }

        Some(BatchSubcommand::Callers { locations }) => {
            let results = execute_batch(locations, BatchOp::Callers, args.parallel, app).await;
            ctx.print_success_flat(results);
        }

        Some(BatchSubcommand::Callees { locations }) => {
            let results = execute_batch(locations, BatchOp::Callees, args.parallel, app).await;
            ctx.print_success_flat(results);
        }

        Some(BatchSubcommand::Stdin) | None => {
            execute_stdin_mode(args.parallel, args.fail_fast, app).await?;
        }
    }

    Ok(())
}

async fn execute_batch(
    locations: Vec<String>,
    op: BatchOp,
    parallel: bool,
    app: &App,
) -> BatchResponse {
    let mut results = Vec::with_capacity(locations.len());

    if parallel {
        let futures: Vec<_> = locations
            .iter()
            .enumerate()
            .map(|(i, loc)| execute_single_op(loc, &op, app, i))
            .collect();

        results = futures::future::join_all(futures).await;
    } else {
        for (index, loc) in locations.iter().enumerate() {
            results.push(execute_single_op(loc, &op, app, index).await);
        }
    }

    let successes = results.iter().filter(|r| r.success).count();
    BatchResponse {
        total: results.len(),
        successes,
        failures: results.len() - successes,
        results,
    }
}

async fn execute_single_op(loc_str: &str, op: &BatchOp, app: &App, index: usize) -> BatchResult {
    let loc = match ParsedLocation::parse(loc_str).and_then(|p| p.to_absolute()) {
        Ok(l) => l,
        Err(e) => {
            return BatchResult {
                index,
                success: false,
                location: Some(loc_str.to_string()),
                result: None,
                error: Some(e.to_string()),
            };
        }
    };

    let result = match op {
        BatchOp::Hover => app
            .lsp
            .hover(&loc.file, loc.line, loc.column)
            .await
            .map(|h| {
                serde_json::json!({
                    "content": h.map(|hov| hov.content)
                })
            }),
        BatchOp::Refs => {
            let ctx = &app.output;
            app.lsp
                .find_references(&loc.file, loc.line, loc.column)
                .await
                .map(|refs| {
                    let project_refs: Vec<_> = refs
                        .iter()
                        .filter(|r| ctx.is_project_path(&r.file))
                        .take(50)
                        .collect();
                    serde_json::json!({
                        "count": project_refs.len(),
                        "references": project_refs.iter().map(|r| serde_json::json!({
                            "file": r.file.display().to_string(),
                            "line": r.line,
                        })).collect::<Vec<_>>()
                    })
                })
        }
        BatchOp::Def => app
            .lsp
            .goto_definition(&loc.file, loc.line, loc.column)
            .await
            .map(|d| {
                d.map(|def| {
                    serde_json::json!({
                        "file": def.file.display().to_string(),
                        "line": def.line,
                        "column": def.column,
                    })
                })
                .unwrap_or(serde_json::json!({ "definition": null }))
            }),
        BatchOp::Callers => app
            .lsp
            .incoming_calls(&loc.file, loc.line, loc.column)
            .await
            .map(|calls| {
                serde_json::json!({
                    "count": calls.len(),
                    "callers": calls.iter().map(|c| serde_json::json!({
                        "name": c.name,
                        "file": c.location.file.display().to_string(),
                        "line": c.location.line,
                    })).collect::<Vec<_>>()
                })
            }),
        BatchOp::Callees => app
            .lsp
            .outgoing_calls(&loc.file, loc.line, loc.column)
            .await
            .map(|calls| {
                serde_json::json!({
                    "count": calls.len(),
                    "callees": calls.iter().map(|c| serde_json::json!({
                        "name": c.name,
                        "file": c.location.file.display().to_string(),
                        "line": c.location.line,
                    })).collect::<Vec<_>>()
                })
            }),
    };

    match result {
        Ok(data) => BatchResult {
            index,
            success: true,
            location: Some(loc_str.to_string()),
            result: Some(data),
            error: None,
        },
        Err(e) => BatchResult {
            index,
            success: false,
            location: Some(loc_str.to_string()),
            result: None,
            error: Some(e.to_string()),
        },
    }
}

async fn execute_batch_by_symbols(
    file: &str,
    pattern: &str,
    parallel: bool,
    app: &App,
) -> Result<BatchResponse> {
    let path = Path::new(file);
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        app.root().join(path)
    };

    let mut symbols = app
        .lsp
        .find_symbols(&abs_path, FindSymbolsOptions::new().with_depth(10))
        .await?;
    Symbol::compute_paths_for_all(&mut symbols);

    let matched = Symbol::filter_by_path(&symbols, pattern);
    let locations: Vec<String> = matched
        .iter()
        .map(|s| {
            format!(
                "{}:{}:{}",
                s.location.file.display(),
                s.location.line,
                s.location.column
            )
        })
        .collect();

    Ok(execute_batch(locations, BatchOp::Refs, parallel, app).await)
}

async fn execute_stdin_mode(parallel: bool, fail_fast: bool, app: &App) -> Result<()> {
    let ctx = &app.output;

    let stdin = io::stdin();
    let commands: Vec<StdinCommand> = stdin
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

    if parallel {
        let futures: Vec<_> = commands
            .iter()
            .enumerate()
            .map(|(i, cmd)| execute_stdin_command(cmd, app, i))
            .collect();

        results = futures::future::join_all(futures).await;
    } else {
        for (index, cmd) in commands.iter().enumerate() {
            let result = execute_stdin_command(cmd, app, index).await;
            if !result.success && fail_fast {
                results.push(result);
                break;
            }
            results.push(result);
        }
    }

    let successes = results.iter().filter(|r| r.success).count();
    ctx.print_success_flat(BatchResponse {
        total: results.len(),
        successes,
        failures: results.len() - successes,
        results,
    });

    Ok(())
}

async fn execute_stdin_command(cmd: &StdinCommand, app: &App, index: usize) -> BatchResult {
    match execute_stdin_command_inner(cmd, app).await {
        Ok(data) => BatchResult {
            index,
            success: true,
            location: None,
            result: Some(data),
            error: None,
        },
        Err(e) => BatchResult {
            index,
            success: false,
            location: None,
            result: None,
            error: Some(e.to_string()),
        },
    }
}

async fn execute_stdin_command_inner(
    cmd: &StdinCommand,
    app: &App,
) -> Result<serde_json::Value, anyhow::Error> {
    let ctx = &app.output;

    match cmd {
        StdinCommand::FindSymbol { file } => {
            let path = std::env::current_dir().unwrap().join(file);
            app.lsp
                .find_symbols(&path, FindSymbolsOptions::default())
                .await
                .map(|symbols| {
                    serde_json::json!({
                        "count": symbols.len(),
                        "symbols": symbols.iter().map(|s| serde_json::json!({
                            "name": s.name,
                            "kind": s.kind.to_string(),
                            "line": s.location.line,
                        })).collect::<Vec<_>>()
                    })
                })
                .map_err(Into::into)
        }

        StdinCommand::FindRefs { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let refs = app
                .lsp
                .find_references(&loc.file, loc.line, loc.column)
                .await?;

            let project_refs: Vec<_> = refs
                .iter()
                .filter(|l| ctx.is_project_path(&l.file))
                .collect();

            Ok(serde_json::json!({
                "count": project_refs.len(),
                "references": project_refs.iter().map(|r| serde_json::json!({
                    "file": r.file.display().to_string(),
                    "line": r.line,
                })).collect::<Vec<_>>()
            }))
        }

        StdinCommand::FindDef { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let def = app
                .lsp
                .goto_definition(&loc.file, loc.line, loc.column)
                .await?;

            Ok(def
                .map(|d| {
                    serde_json::json!({
                        "file": d.file.display().to_string(),
                        "line": d.line,
                        "column": d.column,
                    })
                })
                .unwrap_or(serde_json::json!({ "definition": null })))
        }

        StdinCommand::FindTypedef { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let def = app
                .lsp
                .goto_type_definition(&loc.file, loc.line, loc.column)
                .await?;

            Ok(def
                .map(|d| {
                    serde_json::json!({
                        "file": d.file.display().to_string(),
                        "line": d.line,
                        "column": d.column,
                    })
                })
                .unwrap_or(serde_json::json!({ "definition": null })))
        }

        StdinCommand::FindImpl { location } => {
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
                })).collect::<Vec<_>>()
            }))
        }

        StdinCommand::Hover { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let hover = app.lsp.hover(&loc.file, loc.line, loc.column).await?;

            Ok(serde_json::json!({
                "content": hover.map(|h| h.content)
            }))
        }

        StdinCommand::Diagnostics { file } => {
            let path = std::env::current_dir()?.join(file);
            let diags = app.lsp.diagnostics(&path).await?;

            Ok(serde_json::json!({
                "count": diags.len(),
                "diagnostics": diags.iter().map(|d| serde_json::json!({
                    "message": d.message,
                    "severity": format!("{:?}", d.severity),
                    "line": d.range.start.line + 1,
                })).collect::<Vec<_>>()
            }))
        }

        StdinCommand::Rename { location, new_name } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let result = app
                .lsp
                .rename(&loc.file, loc.line, loc.column, new_name)
                .await?;

            Ok(serde_json::json!({
                "files_changed": result.changes.len(),
                "changes": result.changes.iter().map(|c| serde_json::json!({
                    "file": c.file.display().to_string(),
                    "edits": c.edit_count,
                })).collect::<Vec<_>>()
            }))
        }

        StdinCommand::CallsIncoming { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let calls = app
                .lsp
                .incoming_calls(&loc.file, loc.line, loc.column)
                .await?;

            Ok(serde_json::json!({
                "count": calls.len(),
                "callers": calls.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "file": c.location.file.display().to_string(),
                    "line": c.location.line,
                })).collect::<Vec<_>>()
            }))
        }

        StdinCommand::CallsOutgoing { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let calls = app
                .lsp
                .outgoing_calls(&loc.file, loc.line, loc.column)
                .await?;

            Ok(serde_json::json!({
                "count": calls.len(),
                "callees": calls.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "file": c.location.file.display().to_string(),
                    "line": c.location.line,
                })).collect::<Vec<_>>()
            }))
        }

        StdinCommand::Context { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let (hover, refs) = tokio::join!(
                app.lsp.hover(&loc.file, loc.line, loc.column),
                app.lsp.find_references(&loc.file, loc.line, loc.column)
            );

            Ok(serde_json::json!({
                "hover": hover.ok().flatten().map(|h| h.content),
                "references_count": refs.map(|r| r.len()).unwrap_or(0),
            }))
        }

        StdinCommand::Expand { location } => {
            let loc = ParsedLocation::parse(location)?.to_absolute()?;
            let def = app
                .lsp
                .goto_definition(&loc.file, loc.line, loc.column)
                .await?;

            match def {
                Some(d) => {
                    let symbols = app
                        .lsp
                        .find_symbols(&d.file, FindSymbolsOptions::new().with_body())
                        .await?;
                    let sym = symbols.iter().find(|s| s.location.line == d.line);

                    Ok(serde_json::json!({
                        "definition": {
                            "file": d.file.display().to_string(),
                            "line": d.line,
                        },
                        "body": sym.and_then(|s| s.body.clone()),
                    }))
                }
                None => Ok(serde_json::json!({ "definition": null })),
            }
        }
    }
}
