//! Hover command implementation
//!
//! Get hover information (type, documentation) for a position.

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{HoverResponse, LocationOutput};

#[derive(Args, Debug)]
pub struct HoverArgs {
    /// File path with position (file:line:column)
    pub location: String,
}

pub async fn execute(args: HoverArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app.lsp.hover(&loc.file, loc.line, loc.column).await {
        Ok(Some(info)) => {
            let response = HoverResponse {
                content: Some(info.content),
                range: info
                    .range
                    .map(|r| LocationOutput::from_path(&r.file, r.line, r.column, ctx.root())),
                message: None,
            };
            ctx.print_success_flat(response);
        }
        Ok(None) => {
            let response = HoverResponse {
                content: None,
                range: None,
                message: Some("No hover information available".to_string()),
            };
            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
