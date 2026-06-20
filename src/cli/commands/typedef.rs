use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::commands::common::execute_optional;
use crate::cli::response::{DefinitionOutput, LocationOutput};

#[derive(Args, Debug)]
pub struct TypedefArgs {
    #[command(flatten)]
    pub loc: LocationArg,
}

pub async fn execute(args: TypedefArgs, app: &App) -> Result<()> {
    execute_optional(
        app,
        args.loc,
        |file, line, col| async move { app.lsp.goto_type_definition(&file, line, col).await },
        |def, ctx| DefinitionOutput {
            definition: Some(LocationOutput::from_location(&def, ctx.root())),
            message: None,
        },
        || DefinitionOutput {
            definition: None,
            message: Some("No type definition found".to_string()),
        },
    )
    .await
}
