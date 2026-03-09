use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::commands::common::execute_list;
use crate::cli::response::CallHierarchyOutput;

#[derive(Args, Debug)]
pub struct CalleesArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: CalleesArgs, app: &App) -> Result<()> {
    let limit = args.limit.unwrap_or(app.config().lsp.calls_limit);

    execute_list(
        app,
        args.loc,
        limit,
        |file, line, col| async move { app.lsp.outgoing_calls(&file, line, col).await },
        |c, root| CallHierarchyOutput::from_item(&c, root),
    )
    .await
}
