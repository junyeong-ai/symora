use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::commands::common::execute_list;
use crate::cli::response::TypeInfoOutput;

#[derive(Args, Debug)]
pub struct SubtypesArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: SubtypesArgs, app: &App) -> Result<()> {
    let limit = args.limit.unwrap_or(app.config().lsp.type_hierarchy_limit);

    execute_list(
        app,
        args.loc,
        limit,
        |file, line, col| async move { app.lsp.subtypes(&file, line, col).await },
        |t, root| TypeInfoOutput::from_item(&t, root),
    )
    .await
}
