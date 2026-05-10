use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::Section;

#[derive(Args, Debug)]
pub struct CodeLensArgs {
    /// File to get code lenses for
    pub file: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct CodeLensOutput {
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

pub async fn execute(args: CodeLensArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let file = if args.file.is_absolute() {
        args.file.clone()
    } else {
        app.root().join(&args.file)
    };

    match app.lsp.code_lenses(&file).await {
        Ok(lenses) => {
            let items: Vec<CodeLensOutput> = lenses
                .into_iter()
                .map(|l| CodeLensOutput {
                    start_line: l.range.start.line + 1,
                    end_line: l.range.end.line + 1,
                    title: l.command.as_ref().map(|c| c.title.clone()),
                    command: l.command.as_ref().map(|c| c.command.clone()),
                })
                .collect();
            ctx.print_success(Section::new(items));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
