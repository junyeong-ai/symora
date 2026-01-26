//! Subtypes command - Find child types (subclasses, implementations)

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{Section, TypeHierarchyOutput};

#[derive(Args, Debug)]
pub struct SubtypesArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: SubtypesArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.calls_limit);
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app.lsp.subtypes(&loc.file, loc.line, loc.column).await {
        Ok(items) => {
            let total = items.len();
            let output: Vec<TypeHierarchyOutput> = items
                .into_iter()
                .take(limit)
                .map(|t| TypeHierarchyOutput::from_item(&t, ctx.root()))
                .collect();

            ctx.print_success_flat(Section::with_limit(output, total));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
