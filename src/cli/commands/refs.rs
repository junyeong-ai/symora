//! Refs command - find all references to a symbol

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{LocationOutput, Section};
use crate::cli::utils::read_line_at;

#[derive(Args, Debug)]
pub struct RefsArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// Include source code snippet
    #[arg(long)]
    pub snippet: bool,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: RefsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.refs_limit);
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app
        .lsp
        .find_references(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(locations) => {
            let project_refs: Vec<_> = locations
                .iter()
                .filter(|l| ctx.is_project_path(&l.file))
                .collect();

            let total = project_refs.len();

            let items: Vec<LocationOutput> = project_refs
                .into_iter()
                .take(limit)
                .map(|l| {
                    let mut output =
                        LocationOutput::from_path(&l.file, l.line, l.column, ctx.root());
                    if args.snippet
                        && let Ok(s) = read_line_at(&l.file, l.line)
                    {
                        output.snippet = Some(s);
                    }
                    output
                })
                .collect();

            ctx.print_success_flat(Section::with_limit(items, total));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
