use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::Section;

#[derive(Args, Debug)]
pub struct FoldingArgs {
    /// File to get folding ranges for
    pub file: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct FoldingRangeOutput {
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_character: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_character: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapsed_text: Option<String>,
}

pub async fn execute(args: FoldingArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let file = if args.file.is_absolute() {
        args.file.clone()
    } else {
        app.root().join(&args.file)
    };

    match app.lsp.folding_ranges(&file).await {
        Ok(ranges) => {
            let items: Vec<FoldingRangeOutput> = ranges
                .into_iter()
                .map(|r| FoldingRangeOutput {
                    start_line: r.start_line + 1,
                    end_line: r.end_line + 1,
                    start_character: r.start_character.map(|c| c + 1),
                    end_character: r.end_character.map(|c| c + 1),
                    kind: Some(r.kind.to_string()),
                    collapsed_text: r.collapsed_text,
                })
                .collect();
            ctx.print_success(Section::new(items));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
