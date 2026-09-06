use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::commands::common::execute_optional;
use crate::cli::response::{ParameterOutput, SignatureHelpOutput, SignatureItemOutput};

#[derive(Args, Debug)]
pub struct SignatureArgs {
    #[command(flatten)]
    pub loc: LocationArg,
}

pub async fn execute(args: SignatureArgs, app: &App) -> Result<()> {
    execute_optional(
        app,
        args.loc,
        |file, line, col| async move { app.lsp.signature_help(&file, line, col).await },
        |help, _ctx| SignatureHelpOutput {
            signatures: help
                .signatures
                .into_iter()
                .map(|s| SignatureItemOutput {
                    label: s.label,
                    documentation: s.documentation,
                    parameters: s
                        .parameters
                        .into_iter()
                        .map(|p| ParameterOutput {
                            label: p.label,
                            documentation: p.documentation,
                        })
                        .collect(),
                    active_parameter: s.active_parameter,
                })
                .collect(),
            active_signature: help.active_signature,
            active_parameter: help.active_parameter,
            message: None,
            indexing: None,
        },
        || SignatureHelpOutput {
            signatures: vec![],
            active_signature: None,
            active_parameter: None,
            message: Some("No signature help available".to_string()),
            indexing: None,
        },
    )
    .await
}
