//! Calls command implementation
//!
//! Call hierarchy operations using LSP.

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::output::OutputContext;
use crate::cli::response::{CallHierarchyOutput, CallsResponse, LocationOutput};
use crate::models::lsp::CallHierarchyItem;

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
    let (location, limit, direction) = match args.command {
        CallsCommand::Incoming { location, limit } => (
            location,
            limit.unwrap_or(cfg.lsp.calls_limit),
            Direction::Incoming,
        ),
        CallsCommand::Outgoing { location, limit } => (
            location,
            limit.unwrap_or(cfg.lsp.calls_limit),
            Direction::Outgoing,
        ),
    };

    execute_calls(&location, limit, direction, app).await
}

async fn execute_calls(
    location: &str,
    limit: usize,
    direction: Direction,
    app: &App,
) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(location)?.to_absolute()?;

    let result = match direction {
        Direction::Incoming => {
            app.lsp
                .incoming_calls(&loc.file, loc.line, loc.column)
                .await
        }
        Direction::Outgoing => {
            app.lsp
                .outgoing_calls(&loc.file, loc.line, loc.column)
                .await
        }
    };

    match result {
        Ok(calls) => {
            let limited: Vec<CallHierarchyItem> = calls.into_iter().take(limit).collect();
            let response = build_response(direction, limited, ctx);
            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

fn build_response(
    direction: Direction,
    calls: Vec<CallHierarchyItem>,
    ctx: &OutputContext,
) -> CallsResponse {
    CallsResponse {
        direction: direction.as_str().to_string(),
        count: calls.len(),
        calls: calls
            .iter()
            .map(|c| CallHierarchyOutput {
                name: c.name.clone(),
                location: LocationOutput::from_path(
                    &c.location.file,
                    c.location.line,
                    c.location.column,
                    ctx.root(),
                ),
                call_site: c.call_site.as_ref().map(|site| {
                    LocationOutput::from_path(&site.file, site.line, site.column, ctx.root())
                }),
            })
            .collect(),
    }
}
