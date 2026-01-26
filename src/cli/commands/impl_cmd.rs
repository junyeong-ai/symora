//! Impl command - find implementations of a trait/interface

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{LocationOutput, Section};

#[derive(Args, Debug)]
pub struct ImplArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: ImplArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.impl_limit);
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app
        .lsp
        .find_implementations(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(locations) => {
            let total = locations.len();
            let items: Vec<LocationOutput> = locations
                .into_iter()
                .take(limit)
                .map(|l| LocationOutput::from_path(&l.file, l.line, l.column, ctx.root()))
                .collect();

            ctx.print_success_flat(Section::with_limit(items, total));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
