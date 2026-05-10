use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::Section;

#[derive(Args, Debug)]
pub struct SelectionArgs {
    #[command(flatten)]
    pub loc: LocationArg,
}

#[derive(Debug, Serialize)]
pub struct SelectionRangeOutput {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Box<SelectionRangeOutput>>,
}

fn to_output(r: &crate::models::lsp::SelectionRange) -> SelectionRangeOutput {
    SelectionRangeOutput {
        start_line: r.range.start.line + 1,
        start_character: r.range.start.character + 1,
        end_line: r.range.end.line + 1,
        end_character: r.range.end.character + 1,
        parent: r.parent.as_ref().map(|p| Box::new(to_output(p))),
    }
}

pub async fn execute(args: SelectionArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = args.loc.parse()?.to_absolute()?;

    let positions = vec![(loc.line, loc.column)];

    match app.lsp.selection_ranges(&loc.file, positions).await {
        Ok(ranges) => {
            let items: Vec<SelectionRangeOutput> = ranges.iter().map(to_output).collect();
            ctx.print_success(Section::new(items));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
