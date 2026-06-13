use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::Section;

#[derive(Args, Debug)]
pub struct InlayHintsArgs {
    /// File to get inlay hints for
    pub file: PathBuf,

    /// Start line (1-indexed, default: 1)
    #[arg(long, default_value = "1")]
    pub start_line: u32,

    /// End line (1-indexed, default: end of file)
    #[arg(long)]
    pub end_line: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct InlayHintOutput {
    pub line: u32,
    pub character: u32,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub padding_left: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub padding_right: bool,
}

fn is_false(v: &bool) -> bool {
    !v
}

pub async fn execute(args: InlayHintsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let file = if args.file.is_absolute() {
        args.file.clone()
    } else {
        app.root().join(&args.file)
    };
    // The surface is line-granular and 1-indexed; the LSP wire is 0-indexed.
    let start_line = args.start_line.saturating_sub(1);
    let end_line = args.end_line.unwrap_or(u32::MAX).saturating_sub(1);

    match app.lsp.inlay_hints(&file, start_line, end_line).await {
        Ok(hints) => {
            let items: Vec<InlayHintOutput> = hints
                .into_iter()
                .map(|h| InlayHintOutput {
                    line: h.position.line + 1,
                    character: h.position.character + 1,
                    label: h.label,
                    kind: match h.kind {
                        crate::models::lsp::InlayHintKind::Type => Some("type".to_string()),
                        crate::models::lsp::InlayHintKind::Parameter => {
                            Some("parameter".to_string())
                        }
                    },
                    padding_left: h.padding_left,
                    padding_right: h.padding_right,
                })
                .collect();
            ctx.print_success(Section::new(items));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
