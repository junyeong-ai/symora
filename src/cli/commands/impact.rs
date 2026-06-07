use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::blast_radius::{self, BlastRadiusConfig};
use crate::cli::response::{
    AffectedFileOutput, ImpactOutput, RefOutput, TargetOutput, TestCoverageOutput,
};
use crate::constants::defaults::{IMPACT_DEFAULT_DEPTH, IMPACT_FILES_LIMIT, IMPACT_MAX_DEPTH};

#[derive(Args, Debug)]
pub struct ImpactArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum affected files to list
    #[arg(long, default_value_t = IMPACT_FILES_LIMIT)]
    pub limit: usize,

    /// Transitive caller depth (1 = direct callers only, max 3).
    /// Each extra hop costs an LSP round-trip per caller, so prefer 1
    /// for surveys and 2-3 only when ranking blast radius matters.
    #[arg(long, default_value_t = IMPACT_DEFAULT_DEPTH)]
    pub depth: u32,
}

pub async fn execute(args: ImpactArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = args.loc.parse()?.to_absolute()?;
    let test_matcher = app.test_matcher();
    let root = ctx.root();
    let depth = args.depth.clamp(1, IMPACT_MAX_DEPTH);

    match LocationAnalysis::at(app.lsp.as_ref(), loc).await {
        Ok(analysis) => {
            let classified = analysis.classify(root, test_matcher, false);

            let mut affected_files: Vec<AffectedFileOutput> = classified
                .file_counts
                .into_iter()
                .map(|(path, (is_test, refs))| AffectedFileOutput {
                    file: ctx.relative_path(&path),
                    is_test,
                    refs,
                })
                .collect();
            affected_files.sort_by_key(|f| std::cmp::Reverse(f.refs));
            let total_files = affected_files.len();
            affected_files.truncate(args.limit);

            let test_files: Vec<String> = classified
                .test_refs
                .iter()
                .map(|r| ctx.relative_path(&r.file))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();

            let target = TargetOutput::from_symbol_or_fallback(
                analysis.target.as_ref(),
                &analysis.anchor.file,
                analysis.anchor.line,
                analysis.anchor.column,
                root,
            );

            let exported = analysis.is_exported();
            let anchor_kind = analysis.target().map(|symbol| symbol.kind);

            let blast_radius = blast_radius::compute(
                app.lsp.as_ref(),
                &analysis.anchor.file,
                analysis.anchor.line,
                analysis.anchor.column,
                exported,
                anchor_kind,
                test_matcher,
                &BlastRadiusConfig {
                    max_depth: depth,
                    ..Default::default()
                },
            )
            .await
            .ok();

            let response = ImpactOutput {
                target,
                refs: RefOutput {
                    total: classified.total,
                    test: classified.test,
                    prod: classified.prod,
                    files: Some(total_files),
                    modules: Some(classified.unique_modules),
                    is_exported: exported,
                },
                coverage: TestCoverageOutput {
                    count: classified.test,
                    files: test_files,
                },
                files: affected_files,
                blast_radius,
            };

            ctx.print_success(response);
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
