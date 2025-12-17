//! Impact command implementation
//!
//! Analyze the impact of changing a symbol using LSP references.

use std::collections::HashMap;

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{
    ImpactFileOutput, ImpactReferenceOutput, ImpactResponse, LocationOutput,
};

#[derive(Args, Debug)]
pub struct ImpactArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// Analysis depth (how many levels of callers to trace)
    #[arg(short, long, default_value = "1")]
    pub depth: u32,
}

pub async fn execute(args: ImpactArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app
        .lsp
        .find_references(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(references) => {
            // Filter to project-only references
            let project_refs: Vec<_> = references
                .iter()
                .filter(|r| ctx.is_project_path(&r.file))
                .collect();

            // Group references by file
            let mut files: HashMap<String, Vec<ImpactReferenceOutput>> = HashMap::new();

            for r in &project_refs {
                let file_str = ctx.relative_path(&r.file);
                files
                    .entry(file_str)
                    .or_default()
                    .push(ImpactReferenceOutput {
                        line: r.line,
                        column: r.column,
                    });
            }

            let affected_files: Vec<_> = files
                .into_iter()
                .map(|(file, refs)| ImpactFileOutput {
                    file,
                    reference_count: refs.len(),
                    references: refs,
                })
                .collect();

            let response = ImpactResponse {
                target: LocationOutput::from_path(&loc.file, loc.line, loc.column, ctx.root()),
                depth: args.depth,
                total_references: project_refs.len(),
                affected_files_count: affected_files.len(),
                affected_files,
            };

            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
