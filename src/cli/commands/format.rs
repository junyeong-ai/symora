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
                let content = tokio::fs::read_to_string(&file).await?;
                let formatted = apply_edits(&content, &edits);
                tokio::fs::write(&file, &formatted).await?;
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

fn apply_edits(content: &str, edits: &[crate::models::lsp::TextEdit]) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut result = content.to_string();

    // Apply edits in reverse order to preserve positions
    let mut sorted_edits: Vec<_> = edits.iter().collect();
    sorted_edits.sort_by(|a, b| {
        b.range
            .start
            .line
            .cmp(&a.range.start.line)
            .then(b.range.start.character.cmp(&a.range.start.character))
    });

    for edit in sorted_edits {
        let start_offset = line_col_to_offset(
            &lines,
            content,
            edit.range.start.line,
            edit.range.start.character,
        );
        let end_offset = line_col_to_offset(
            &lines,
            content,
            edit.range.end.line,
            edit.range.end.character,
        );

        if let (Some(start), Some(end)) = (start_offset, end_offset) {
            result.replace_range(start..end, &edit.new_text);
        }
    }

    result
}

fn line_col_to_offset(lines: &[&str], content: &str, line: u32, col: u32) -> Option<usize> {
    let line = line as usize;
    let col = col as usize;

    if line >= lines.len() {
        return Some(content.len());
    }

    let mut offset = 0;
    for (i, l) in lines.iter().enumerate() {
        if i == line {
            return Some(offset + col.min(l.len()));
        }
        offset += l.len() + 1; // +1 for newline
    }

    Some(content.len())
}
