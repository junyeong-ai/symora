use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::response::{LocationOutput, RefsOutput, Section, TargetOutput};
use crate::cli::symbol_discovery::is_single_file_concentration;
use crate::cli::utils::{extract_signature, read_line_at, read_lines_around};

use super::common::lsp_error_at;

#[derive(Args, Debug)]
pub struct RefsArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Include source code snippet
    #[arg(long)]
    pub snippet: bool,

    /// Include N lines of surrounding context (overrides --snippet)
    #[arg(long)]
    pub context: Option<usize>,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: RefsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.refs_limit);
    let loc = args.loc.parse()?.to_absolute()?;
    // Kept for the error path: `at` consumes the anchor, and the hint reads
    // best against the position the agent actually typed.
    let (err_file, err_line, err_column) = (loc.file.clone(), loc.line, loc.column);

    match LocationAnalysis::at(app.lsp.as_ref(), loc, ctx.root()).await {
        Ok(analysis) => {
            let root = ctx.root();
            let usages = analysis.references();
            let total = usages.len();

            let items: Vec<LocationOutput> = usages
                .iter()
                .take(limit)
                .map(|l| {
                    let mut output = LocationOutput::from_location(l, root);
                    if let Some(n) = args.context {
                        if let Ok(s) = read_lines_around(&l.file, l.line, n) {
                            output.snippet = Some(s);
                        }
                    } else if args.snippet
                        && let Ok(s) = read_line_at(&l.file, l.line)
                    {
                        output.snippet = Some(s);
                    }
                    output
                })
                .collect();

            // Disclose the symbol the input resolved to. The signature reuses
            // the body `LocationAnalysis::at` already fetched, so it costs no
            // extra lookup; it is surfaced once at the top level, never per
            // reference.
            let target = TargetOutput::from_symbol_or_fallback(
                analysis.target(),
                &analysis.anchor().file,
                analysis.anchor().line,
                analysis.anchor().column,
                root,
                analysis.anchor_resolution().as_status(),
            )
            .with_signature(
                analysis
                    .target()
                    .and_then(|symbol| extract_signature(symbol.body.as_deref())),
            );

            let anchor = format!(
                "{}:{}:{}",
                ctx.relative_path(&analysis.anchor().file),
                analysis.anchor().line,
                analysis.anchor().column,
            );
            let next_commands = refs_next_commands(
                &items,
                total,
                limit,
                &anchor,
                analysis.anchor().is_resolved(),
            );
            // Captured when the reference query ran (LocationAnalysis),
            // not re-read here — quiescence landing mid-request must not
            // strip the marker from a lower-bound answer.
            let indexing = analysis.indexing();
            let incomplete = analysis.omits_input(root);
            // The lower-bound cause leads, ahead of the anchor's own hints:
            // what the position resolved to matters less than the count being
            // short, and the contract has this cause lead on every surface.
            let mut hints = Vec::new();
            if incomplete {
                hints.push(format!(
                    "The language server's reference set for this declaration does not include \
                     the position given ({}:{}:{}), which is a usage of it; the count is a lower \
                     bound — the server left out at least that usage",
                    ctx.relative_path(&analysis.anchor().input.file),
                    analysis.anchor().input.line,
                    analysis.anchor().input.column,
                ));
            }
            hints.extend(analysis.member_reach_hint());
            hints.extend(analysis.anchor().anchor_hints(root, "references"));
            ctx.print_success(RefsOutput {
                target,
                references: Section::with_total(items, total)
                    .with_hints(hints)
                    .with_next_commands(next_commands)
                    .with_indexing(indexing)
                    .with_incomplete(incomplete),
            });
        }
        Err(e) => ctx.print_error(lsp_error_at(
            e,
            &ctx.relative_path(&err_file),
            err_line,
            err_column,
        )),
    }

    Ok(())
}

/// Gated follow-up commands for a reference list, in fixed priority order:
/// no usages at all is what `usage` can still answer for a listed symbol,
/// since it searches by name across the workspace rather than from one
/// resolved position (a binding or a position that resolved to nothing has
/// no name for it to take); a truncated multi-file spread is summarized
/// whole by one `impact` call (cheaper than re-paying the reference query
/// with a raised limit); a multi-reference set concentrated in one file —
/// whether complete or truncated — reads best through `context --all`
/// there. The truncation gate requires the spread (`unique_files > 1`) so a
/// single-file truncation steers to `context`, never bouncing between
/// `refs` and `impact`.
fn refs_next_commands(
    items: &[LocationOutput],
    total: usize,
    limit: usize,
    anchor: &str,
    anchored: bool,
) -> Vec<String> {
    let unique_files = items
        .iter()
        .map(|item| item.file.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();

    let mut commands = Vec::new();
    if total == 0 && anchored {
        commands.push(format!("symora usage {anchor}"));
    }
    if total > limit && unique_files > 1 {
        commands.push(format!("symora impact {anchor}"));
    }
    if is_single_file_concentration(unique_files, total) {
        commands.push(format!("symora context {anchor} --all"));
    }
    commands.truncate(2);
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(file: &str, line: u32) -> LocationOutput {
        LocationOutput {
            file: file.to_string(),
            line,
            column: 5,
            snippet: None,
            degraded_column: None,
        }
    }

    #[test]
    fn no_usages_steers_to_usage() {
        assert_eq!(
            refs_next_commands(&[], 0, 20, "src/main.rs:10:5", true),
            vec!["symora usage src/main.rs:10:5"]
        );
    }

    /// A zero from a position that resolved to no declaration is not steered
    /// to `usage`: that command would search for the position string itself.
    #[test]
    fn an_unanchored_zero_is_not_steered_to_usage() {
        assert!(refs_next_commands(&[], 0, 20, "src/main.rs:10:900", false).is_empty());
    }

    /// One genuine usage is an answer, not a dead end — the list already
    /// holds what a broader search would find.
    #[test]
    fn a_single_usage_emits_nothing() {
        let items = vec![item("src/main.rs", 10)];
        assert!(refs_next_commands(&items, 1, 20, "src/main.rs:10:5", true).is_empty());
    }

    #[test]
    fn multi_file_truncation_steers_to_impact() {
        let items = vec![item("src/a.rs", 1), item("src/b.rs", 2)];
        assert_eq!(
            refs_next_commands(&items, 30, 20, "src/main.rs:10:5", true),
            vec!["symora impact src/main.rs:10:5"]
        );
    }

    #[test]
    fn single_file_truncation_steers_to_context_not_impact() {
        let items = vec![item("src/a.rs", 1), item("src/a.rs", 2)];
        assert_eq!(
            refs_next_commands(&items, 30, 20, "src/main.rs:10:5", true),
            vec!["symora context src/main.rs:10:5 --all"]
        );
    }

    #[test]
    fn single_file_concentration_steers_to_context() {
        let items = vec![
            item("src/a.rs", 1),
            item("src/a.rs", 2),
            item("src/a.rs", 3),
        ];
        assert_eq!(
            refs_next_commands(&items, 3, 20, "src/main.rs:10:5", true),
            vec!["symora context src/main.rs:10:5 --all"]
        );
    }

    #[test]
    fn complete_multi_file_spread_emits_nothing() {
        let items = vec![
            item("src/a.rs", 1),
            item("src/b.rs", 2),
            item("src/a.rs", 3),
        ];
        assert!(refs_next_commands(&items, 3, 20, "src/main.rs:10:5", true).is_empty());
    }
}
