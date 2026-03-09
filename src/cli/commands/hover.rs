use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::commands::common::execute_optional;
use crate::cli::response::{HoverOutput, LocationOutput};

#[derive(Args, Debug)]
pub struct HoverArgs {
    #[command(flatten)]
    pub loc: LocationArg,
}

pub async fn execute(args: HoverArgs, app: &App) -> Result<()> {
    execute_optional(
        app,
        args.loc,
        |file, line, col| async move { app.lsp.hover(&file, line, col).await },
        |info, ctx| HoverOutput {
            content: Some(info.content),
            range: info
                .range
                .map(|r| LocationOutput::from_path(&r.file, r.line, r.column, ctx.root())),
            message: None,
        },
        || HoverOutput {
            content: None,
            range: None,
            message: Some("No hover information available".to_string()),
        },
    )
    .await
}
