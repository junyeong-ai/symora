//! Impact command implementation
//!
//! Analyze the impact of changing a symbol using LSP references.
//! Provides safety hints based on test vs production file distribution.

use std::collections::HashMap;

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{
    ImpactFileOutput, ImpactReferenceOutput, ImpactResponse, LocationOutput, SafetyHint,
};
use crate::cli::utils::is_test_file;

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
            let project_refs: Vec<_> = references
                .iter()
                .filter(|r| ctx.is_project_path(&r.file))
                .collect();

            let mut files: HashMap<String, (bool, Vec<ImpactReferenceOutput>)> = HashMap::new();
            let mut test_refs_count = 0;
            let mut production_refs_count = 0;

            for r in &project_refs {
                let file_str = ctx.relative_path(&r.file);
                let is_test = is_test_file(&r.file);

                if is_test {
                    test_refs_count += 1;
                } else {
                    production_refs_count += 1;
                }

                files
                    .entry(file_str)
                    .or_insert_with(|| (is_test, Vec::new()))
                    .1
                    .push(ImpactReferenceOutput {
                        line: r.line,
                        column: r.column,
                    });
            }

            let affected_files: Vec<_> = files
                .into_iter()
                .map(|(file, (is_test, refs))| ImpactFileOutput {
                    file,
                    is_test,
                    reference_count: refs.len(),
                    references: refs,
                })
                .collect();

            let safety_hint = compute_safety_hint(production_refs_count);

            let response = ImpactResponse {
                target: LocationOutput::from_path(&loc.file, loc.line, loc.column, ctx.root()),
                depth: args.depth,
                safety_hint,
                total_references: project_refs.len(),
                test_refs_count,
                production_refs_count,
                affected_files_count: affected_files.len(),
                affected_files,
            };

            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

fn compute_safety_hint(production_count: usize) -> SafetyHint {
    match production_count {
        0 => SafetyHint::Safe,
        1..=3 => SafetyHint::NeedsReview,
        _ => SafetyHint::PotentiallyBreaking,
    }
}
