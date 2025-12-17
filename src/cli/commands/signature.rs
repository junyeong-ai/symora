//! Signature help command implementation
//!
//! Shows function/method signature and parameter information.

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{ParameterOutput, SignatureHelpResponse, SignatureOutput};

#[derive(Args, Debug)]
pub struct SignatureArgs {
    /// File path with position (file:line:column)
    pub location: String,
}

pub async fn execute(args: SignatureArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app
        .lsp
        .signature_help(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(Some(help)) => {
            let response = SignatureHelpResponse {
                signatures: help
                    .signatures
                    .iter()
                    .map(|s| SignatureOutput {
                        label: s.label.clone(),
                        documentation: s.documentation.clone(),
                        parameters: s
                            .parameters
                            .iter()
                            .map(|p| ParameterOutput {
                                label: p.label.clone(),
                                documentation: p.documentation.clone(),
                            })
                            .collect(),
                        active_parameter: s.active_parameter,
                    })
                    .collect(),
                active_signature: help.active_signature,
                active_parameter: help.active_parameter,
            };
            ctx.print_success_flat(response);
        }
        Ok(None) => {
            ctx.print_success_flat(serde_json::json!({
                "signatures": [],
                "message": "No signature help available at this position"
            }));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
