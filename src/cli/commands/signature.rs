//! Signature command - Get function signature and parameter info at position

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;

#[derive(Args, Debug)]
pub struct SignatureArgs {
    /// File path with position (file:line:column)
    pub location: String,
}

#[derive(Serialize)]
struct SignatureResponse {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    signatures: Vec<SignatureOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_signature: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_parameter: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize)]
struct SignatureOutput {
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parameters: Vec<ParameterOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_parameter: Option<u32>,
}

#[derive(Serialize)]
struct ParameterOutput {
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<String>,
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
            let signatures: Vec<SignatureOutput> = help
                .signatures
                .into_iter()
                .map(|s| SignatureOutput {
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
                .collect();

            let response = SignatureResponse {
                signatures,
                active_signature: help.active_signature,
                active_parameter: help.active_parameter,
                message: None,
            };
            ctx.print_success_flat(response);
        }
        Ok(None) => {
            let response = SignatureResponse {
                signatures: vec![],
                active_signature: None,
                active_parameter: None,
                message: Some("No signature help available".to_string()),
            };
            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
