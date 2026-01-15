//! Expand command - inline expand function definitions at call sites

use std::path::Path;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::LocationOutput;
use crate::cli::utils::{extract_signature, find_symbol_at_line};
use crate::models::lsp::FindSymbolsOptions;
use crate::services::lsp::LspService;

#[derive(Args, Debug)]
pub struct ExpandArgs {
    /// Location of the call to expand (file:line:column)
    pub location: String,

    /// Expansion depth (1 = immediate definition only)
    #[arg(short, long, default_value = "1")]
    pub depth: u32,

    /// Include signature only (no body)
    #[arg(long)]
    pub signature_only: bool,
}

#[derive(Debug, Serialize)]
pub struct ExpandResponse {
    pub call_site: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<ExpandedDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExpandedDefinition {
    pub name: String,
    pub kind: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nested: Vec<ExpandedDefinition>,
}

pub async fn execute(args: ExpandArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    let response = expand_call(
        app.lsp.as_ref(),
        &loc.file,
        loc.line,
        loc.column,
        args.depth,
        args.signature_only,
        ctx.root(),
    )
    .await;

    match response {
        Ok(expand) => ctx.print_success_flat(expand),
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

async fn expand_call(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    depth: u32,
    signature_only: bool,
    root: &Path,
) -> Result<ExpandResponse> {
    let call_site = LocationOutput::from_path(file, line, column, root);

    let def_loc = match lsp.goto_definition(file, line, column).await? {
        Some(loc) => loc,
        None => {
            return Ok(ExpandResponse {
                call_site,
                definition: None,
                message: Some("No definition found at this location".to_string()),
            });
        }
    };

    let symbols = lsp
        .find_symbols(&def_loc.file, FindSymbolsOptions::new().with_body())
        .await?;
    let def_symbol = find_symbol_at_line(&symbols, def_loc.line);

    let definition = if let Some(symbol) = def_symbol {
        let nested = if depth > 1 && !signature_only {
            expand_nested(lsp, symbol, depth - 1, root).await
        } else {
            vec![]
        };

        Some(ExpandedDefinition {
            name: symbol.name.clone(),
            kind: symbol.kind.to_string(),
            location: LocationOutput::from_path(&def_loc.file, def_loc.line, def_loc.column, root),
            signature: extract_signature(symbol.body.as_deref()),
            body: if signature_only {
                None
            } else {
                symbol.body.clone()
            },
            nested,
        })
    } else {
        None
    };

    Ok(ExpandResponse {
        call_site,
        definition,
        message: None,
    })
}

fn expand_nested<'a>(
    lsp: &'a dyn LspService,
    symbol: &'a crate::models::symbol::Symbol,
    remaining_depth: u32,
    root: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ExpandedDefinition>> + Send + 'a>> {
    Box::pin(async move {
        if remaining_depth == 0 {
            return vec![];
        }

        let calls = match lsp
            .outgoing_calls(
                &symbol.location.file,
                symbol.location.line,
                symbol.location.column,
            )
            .await
        {
            Ok(c) => c,
            Err(_) => return vec![],
        };

        let mut nested = Vec::new();

        for call in calls.into_iter().take(5) {
            let symbols = match lsp
                .find_symbols(&call.location.file, FindSymbolsOptions::new().with_body())
                .await
            {
                Ok(s) => s,
                Err(_) => continue,
            };

            if let Some(sym) = find_symbol_at_line(&symbols, call.location.line) {
                let children = if remaining_depth > 1 {
                    expand_nested(lsp, sym, remaining_depth - 1, root).await
                } else {
                    vec![]
                };

                nested.push(ExpandedDefinition {
                    name: sym.name.clone(),
                    kind: sym.kind.to_string(),
                    location: LocationOutput::from_path(
                        &call.location.file,
                        call.location.line,
                        call.location.column,
                        root,
                    ),
                    signature: extract_signature(sym.body.as_deref()),
                    body: sym.body.clone(),
                    nested: children,
                });
            }
        }

        nested
    })
}
