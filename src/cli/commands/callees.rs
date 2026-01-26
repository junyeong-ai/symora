//! Callees command - find outgoing calls (what does this function call?)

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{CallHierarchyOutput, Section};

#[derive(Args, Debug)]
pub struct CalleesArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: CalleesArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.calls_limit);
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app
        .lsp
        .outgoing_calls(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(calls) => {
            let total = calls.len();
            let items: Vec<CallHierarchyOutput> = calls
                .into_iter()
                .take(limit)
                .map(|c| CallHierarchyOutput::from_item(&c, ctx.root()))
                .collect();

            ctx.print_success_flat(Section::with_limit(items, total));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
