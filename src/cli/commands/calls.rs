//! Calls command implementation
//!
//! Call hierarchy operations using LSP with automatic fallback to references.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::output::OutputContext;
use crate::cli::response::CallHierarchyOutput;
use crate::error::LspError;
use crate::models::lsp::{CallHierarchyItem, FindSymbolsOptions};
use crate::services::lsp::find_containing_callable;

#[derive(Args, Debug)]
pub struct CallsArgs {
    #[command(subcommand)]
    pub command: CallsCommand,
}

#[derive(Subcommand, Debug)]
pub enum CallsCommand {
    /// Find incoming calls (who calls this function?)
    Incoming {
        /// File path with position (file:line:column)
        location: String,

        /// Maximum results (default from config: lsp.calls_limit)
        #[arg(long)]
        limit: Option<usize>,

        /// Disable automatic fallback to references when call hierarchy is unsupported
        #[arg(long)]
        no_fallback: bool,
    },

    /// Find outgoing calls (what does this function call?)
    Outgoing {
        /// File path with position (file:line:column)
        location: String,

        /// Maximum results (default from config: lsp.calls_limit)
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Clone, Copy)]
enum Direction {
    Incoming,
    Outgoing,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
        }
    }
}

pub async fn execute(args: CallsArgs, app: &App) -> Result<()> {
    let cfg = app.config();
    match args.command {
        CallsCommand::Incoming {
            location,
            limit,
            no_fallback,
        } => {
            let limit = limit.unwrap_or(cfg.lsp.calls_limit);
            execute_incoming(&location, limit, no_fallback, app).await
        }
        CallsCommand::Outgoing { location, limit } => {
            let limit = limit.unwrap_or(cfg.lsp.calls_limit);
            execute_outgoing(&location, limit, app).await
        }
    }
}

async fn execute_incoming(
    location: &str,
    limit: usize,
    no_fallback: bool,
    app: &App,
) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(location)?.to_absolute()?;

    let result = app
        .lsp
        .incoming_calls(&loc.file, loc.line, loc.column)
        .await;

    match result {
        Ok(calls) => {
            let limited: Vec<CallHierarchyItem> = calls.into_iter().take(limit).collect();
            let response = build_response(Direction::Incoming, limited, None, ctx);
            ctx.print_success_flat(response);
        }
        Err(ref e) if !no_fallback && is_feature_not_supported(e) => {
            // Fallback to references-based caller inference
            match incoming_calls_from_refs(app, &loc.file, loc.line, loc.column, limit).await {
                Ok(calls) => {
                    let response =
                        build_response(Direction::Incoming, calls, Some("refs-fallback"), ctx);
                    ctx.print_success_flat(response);
                }
                Err(fallback_err) => ctx.print_error(&fallback_err.to_string()),
            }
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

async fn execute_outgoing(location: &str, limit: usize, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(location)?.to_absolute()?;

    let result = app
        .lsp
        .outgoing_calls(&loc.file, loc.line, loc.column)
        .await;

    match result {
        Ok(calls) => {
            let limited: Vec<CallHierarchyItem> = calls.into_iter().take(limit).collect();
            let response = build_response(Direction::Outgoing, limited, None, ctx);
            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

fn is_feature_not_supported(err: &LspError) -> bool {
    matches!(err, LspError::FeatureNotSupported { .. })
}

/// Infer incoming calls by finding references and extracting containing callables.
async fn incoming_calls_from_refs(
    app: &App,
    file: &Path,
    line: u32,
    column: u32,
    limit: usize,
) -> Result<Vec<CallHierarchyItem>, LspError> {
    let refs = app.lsp.find_references(file, line, column).await?;

    let mut seen = HashSet::new();
    let mut callers = Vec::new();

    for ref_loc in refs {
        // Skip the definition itself
        if ref_loc.file == file && ref_loc.line == line {
            continue;
        }

        // Get symbols from the file containing the reference
        let symbols = app
            .lsp
            .find_symbols(&ref_loc.file, FindSymbolsOptions::new().with_depth(10))
            .await?;

        // Find the callable containing this reference
        if let Some(caller) = find_containing_callable(&symbols, ref_loc.line) {
            // Deduplicate by name + file + line
            let key = (
                caller.name.clone(),
                caller.location.file.clone(),
                caller.location.line,
            );

            if seen.insert(key) {
                callers.push(CallHierarchyItem {
                    name: caller.name.clone(),
                    kind: caller.kind,
                    location: caller.location.clone(),
                    call_site: Some(ref_loc),
                });

                if callers.len() >= limit {
                    break;
                }
            }
        }
    }

    Ok(callers)
}

fn build_response(
    direction: Direction,
    calls: Vec<CallHierarchyItem>,
    source: Option<&str>,
    ctx: &OutputContext,
) -> serde_json::Value {
    let mut response = serde_json::json!({
        "direction": direction.as_str(),
        "count": calls.len(),
        "calls": calls
            .iter()
            .map(|c| CallHierarchyOutput::from_item(c, ctx.root()))
            .collect::<Vec<_>>(),
    });

    if let Some(src) = source {
        response["source"] = serde_json::json!(src);
    }

    response
}
