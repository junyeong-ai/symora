use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::{LocationOutput, Section};

#[derive(Args, Debug)]
pub struct TypedefArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

/// Every type the server names at the position — a composite type such as
/// `Vec<Foo>` or `str | None` names one per type it is built from — in the
/// shared list shape `implementations` emits. Unlike that command the
/// position is read exactly, as `def` and `hover` read it, so no anchor is
/// resolved and no anchor hint applies. An empty list under `indexing` is a
/// lower bound, never a settled absence.
pub async fn execute(args: TypedefArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let limit = args.limit.unwrap_or(app.config().lsp.typedef_limit);
    let loc = args.loc.parse()?.resolve(None)?;

    match app
        .lsp
        .goto_type_definition(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(result) => {
            let total = result.data.len();
            let items = result
                .data
                .iter()
                .take(limit)
                .map(|location| LocationOutput::from_location(location, ctx.root()))
                .collect();
            ctx.print_success(Section::with_total(items, total).with_indexing(result.indexing));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
