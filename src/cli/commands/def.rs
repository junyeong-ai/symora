//! Def command - go to definition

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{DefinitionResponse, LocationOutput};

#[derive(Args, Debug)]
pub struct DefArgs {
    /// File path with position (file:line:column)
    pub location: String,
}

pub async fn execute(args: DefArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app
        .lsp
        .goto_definition(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(Some(def)) => {
            let response = DefinitionResponse {
                definition: Some(LocationOutput::from_path(
                    &def.file,
                    def.line,
                    def.column,
                    ctx.root(),
                )),
                message: None,
            };
            ctx.print_success_flat(response);
        }
        Ok(None) => {
            let response = DefinitionResponse {
                definition: None,
                message: Some("No definition found".to_string()),
            };
            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
