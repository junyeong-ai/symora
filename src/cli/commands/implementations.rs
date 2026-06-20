use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::commands::common::execute_list;
use crate::cli::response::LocationOutput;

#[derive(Args, Debug)]
pub struct ImplementationsArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: ImplementationsArgs, app: &App) -> Result<()> {
    let limit = args.limit.unwrap_or(app.config().lsp.impl_limit);

    execute_list(
        app,
        args.loc,
        limit,
        "implementations",
        |file, line, col| async move { app.lsp.find_implementations(&file, line, col).await },
        |l, root| LocationOutput::from_location(&l, root),
    )
    .await
}
