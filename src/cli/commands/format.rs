use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::Section;

#[derive(Args, Debug)]
pub struct FormatArgs {
    /// File to format
    pub file: PathBuf,

    /// Apply formatting changes to the file
    #[arg(long)]
    pub apply: bool,
}

#[derive(Debug, Serialize)]
pub struct FormatEditOutput {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub new_text: String,
}

pub async fn execute(args: FormatArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let file = if args.file.is_absolute() {
        args.file.clone()
    } else {
        app.root().join(&args.file)
    };

    match app.lsp.format(&file).await {
        Ok(edits) => {
            if args.apply && !edits.is_empty() {
                let content = std::fs::read_to_string(&file)?;
                // Reuse the one text-edit applier (CRLF- and multibyte-correct,
                // overlap-checked) rather than a second local implementation.
                let formatted = super::edit::apply_text_edits(&content, &edits)?;
                super::edit::atomic_write(&file, &formatted)?;
                ctx.print_success(serde_json::json!({
                    "applied": true,
                    "edits": edits.len(),
                    "file": ctx.relative_path(&file),
                }));
            } else {
                let items: Vec<FormatEditOutput> = edits
                    .into_iter()
                    .map(|e| FormatEditOutput {
                        start_line: e.range.start.line + 1,
                        start_character: e.range.start.character + 1,
                        end_line: e.range.end.line + 1,
                        end_character: e.range.end.character + 1,
                        new_text: e.new_text,
                    })
                    .collect();
                ctx.print_success(Section::new(items));
            }
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
