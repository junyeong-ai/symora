use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::blast_radius::{self, BlastRadius, DispatchStatus};
use crate::cli::call_graph::WalkConfig;
use crate::cli::response::{
    AffectedFileOutput, ImpactOutput, RefOutput, TargetOutput, TestCoverageOutput,
};
use crate::cli::symbol_discovery::is_single_file_concentration;
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
    let limit = normalize_limit(args.limit);

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
            affected_files.truncate(limit);

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
                analysis.anchor_resolution().as_status(),
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
                &WalkConfig {
                    max_depth: depth,
                    ..Default::default()
                },
            )
            .await
            .ok();

            let anchor = format!(
                "{}:{}:{}",
                ctx.relative_path(&analysis.anchor.file),
                analysis.anchor.line,
                analysis.anchor.column,
            );
            let next_commands = impact_next_commands(
                &anchor,
                total_files,
                limit,
                classified.total,
                depth,
                blast_radius.as_ref(),
            );

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
                next_commands,
            };

            ctx.print_success(response);
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

/// Gated follow-up commands, in fixed priority order. Every gate keys off
/// a disclosure the output already carries: `max_depth_reached` means the
/// final frontier still had unexplored callers, so one more depth widens a
/// graph known incomplete (silent at `IMPACT_MAX_DEPTH`); an `incomplete`
/// dynamic dispatch means `find_implementations` returned a non-empty set,
/// so the steered command enumerates exactly the unfolded implementations
/// (`unavailable` is excluded — that lookup already failed, steering to it
/// would dead-end); a truncated file list re-runs with the exact total, so
/// the complete re-run can never re-fire the gate; a single-file
/// concentration reads best through the snippeted reference list there.
/// An `impact` re-run carries the call's other flag whenever it differs
/// from its default, so following one suggestion never silently resets
/// the other dimension and re-fires its gate. Both flags arrive
/// normalized (`depth` clamped to 1..=`IMPACT_MAX_DEPTH`, `limit`
/// floored at 1 where args are read), so a carried flag is always a
/// value worth re-running with.
/// `--limit 0` would empty the file list and make every re-run
/// suggestion carry an always-truncating flag; floor it at the input
/// boundary, like `depth`.
fn normalize_limit(limit: usize) -> usize {
    limit.max(1)
}

fn impact_next_commands(
    anchor: &str,
    total_files: usize,
    limit: usize,
    total_refs: usize,
    depth: u32,
    blast_radius: Option<&BlastRadius>,
) -> Vec<String> {
    let limit_flag = if limit == IMPACT_FILES_LIMIT {
        String::new()
    } else {
        format!(" --limit {limit}")
    };
    let depth_flag = if depth == IMPACT_DEFAULT_DEPTH {
        String::new()
    } else {
        format!(" --depth {depth}")
    };

    let mut commands = Vec::new();
    if blast_radius.is_some_and(|b| b.max_depth_reached) && depth < IMPACT_MAX_DEPTH {
        commands.push(format!(
            "symora impact {anchor} --depth {}{limit_flag}",
            depth + 1
        ));
    }
    if blast_radius
        .and_then(|b| b.dynamic_dispatch)
        .is_some_and(|d| d.status == DispatchStatus::Incomplete)
    {
        commands.push(format!("symora implementations {anchor}"));
    }
    if total_files > limit {
        commands.push(format!(
            "symora impact {anchor} --limit {total_files}{depth_flag}"
        ));
    }
    if is_single_file_concentration(total_files, total_refs) {
        commands.push(format!("symora refs {anchor} --snippet"));
    }
    commands.truncate(3);
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::blast_radius::{DynamicDispatch, RiskLevel};

    fn radius(max_depth_reached: bool, dispatch: Option<DispatchStatus>) -> BlastRadius {
        BlastRadius {
            direct_callers: 2,
            transitive_callers: 4,
            depth: 1,
            max_depth_reached,
            callers_truncated: false,
            indexing: None,
            incomplete: false,
            dynamic_dispatch: dispatch.map(|status| DynamicDispatch {
                status,
                implementations: if status == DispatchStatus::Incomplete {
                    2
                } else {
                    0
                },
            }),
            callers_by_depth: vec![],
            test_coverage_ratio: 0.0,
            risk: RiskLevel::Low,
            confidence: 0.8,
        }
    }

    #[test]
    fn depth_gate_reruns_one_deeper() {
        let radius = radius(true, None);
        let commands = impact_next_commands("src/main.rs:10:5", 3, 50, 8, 1, Some(&radius));
        assert_eq!(commands, vec!["symora impact src/main.rs:10:5 --depth 2"]);
    }

    #[test]
    fn depth_gate_silent_at_max() {
        let radius = radius(true, None);
        let commands = impact_next_commands(
            "src/main.rs:10:5",
            3,
            50,
            8,
            IMPACT_MAX_DEPTH,
            Some(&radius),
        );
        assert!(commands.is_empty());
    }

    #[test]
    fn incomplete_dispatch_steers_to_implementations() {
        let incomplete = radius(false, Some(DispatchStatus::Incomplete));
        assert_eq!(
            impact_next_commands("src/main.rs:10:5", 3, 50, 8, 1, Some(&incomplete)),
            vec!["symora implementations src/main.rs:10:5"]
        );

        let unavailable = radius(false, Some(DispatchStatus::Unavailable));
        assert!(
            impact_next_commands("src/main.rs:10:5", 3, 50, 8, 1, Some(&unavailable)).is_empty()
        );
    }

    #[test]
    fn files_truncation_reruns_with_exact_limit() {
        let commands = impact_next_commands("src/main.rs:10:5", 80, 50, 200, 1, None);
        assert_eq!(commands, vec!["symora impact src/main.rs:10:5 --limit 80"]);
    }

    /// Each `impact` re-run carries the other flag's non-default value, so
    /// an agent following one suggestion can't oscillate between the
    /// depth and limit gates.
    #[test]
    fn depth_rerun_carries_non_default_limit() {
        let radius = radius(true, None);
        let commands = impact_next_commands("src/main.rs:10:5", 3, 10, 8, 1, Some(&radius));
        assert_eq!(
            commands,
            vec!["symora impact src/main.rs:10:5 --depth 2 --limit 10"]
        );
    }

    #[test]
    fn limit_rerun_carries_non_default_depth() {
        let commands = impact_next_commands("src/main.rs:10:5", 80, 50, 200, 2, None);
        assert_eq!(
            commands,
            vec!["symora impact src/main.rs:10:5 --limit 80 --depth 2"]
        );
    }

    #[test]
    fn concentration_steers_to_refs_snippet() {
        let commands = impact_next_commands("src/main.rs:10:5", 1, 50, 5, 1, None);
        assert_eq!(commands, vec!["symora refs src/main.rs:10:5 --snippet"]);
    }

    #[test]
    fn commands_cap_at_three_in_fixed_order() {
        let radius = radius(true, Some(DispatchStatus::Incomplete));
        let commands = impact_next_commands("src/main.rs:10:5", 2, 1, 5, 1, Some(&radius));
        assert_eq!(
            commands,
            vec![
                "symora impact src/main.rs:10:5 --depth 2 --limit 1",
                "symora implementations src/main.rs:10:5",
                "symora impact src/main.rs:10:5 --limit 2",
            ]
        );
    }

    /// `--limit 0` is floored to 1 where args are read, so even with the
    /// depth and limit gates firing together no suggestion can carry
    /// `--limit 0` back into a loop of always-truncated re-runs.
    #[test]
    fn zero_limit_is_normalized_before_both_gates() {
        let normalized = normalize_limit(0);
        let radius = radius(true, None);
        let commands = impact_next_commands("src/main.rs:10:5", 3, normalized, 8, 1, Some(&radius));
        assert_eq!(
            commands,
            vec![
                "symora impact src/main.rs:10:5 --depth 2 --limit 1",
                "symora impact src/main.rs:10:5 --limit 3",
            ]
        );
        assert!(commands.iter().all(|c| !c.contains("--limit 0")));
    }

    #[test]
    fn nothing_fires_emits_empty() {
        let radius = radius(false, None);
        assert!(impact_next_commands("src/main.rs:10:5", 3, 50, 8, 1, Some(&radius)).is_empty());
        assert!(impact_next_commands("src/main.rs:10:5", 3, 50, 8, 1, None).is_empty());
    }
}
