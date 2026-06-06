use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::commands::common::snap_to_symbol_anchor;
use crate::cli::response::{LocationOutput, Section};
use crate::cli::utils::{read_line_at, read_lines_around};
use crate::cli::{LocationArg, OutputError};

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
    let loc = args.loc.parse()?.to_absolute_with_root(Some(app.root()))?;

    let (line, column) =
        snap_to_symbol_anchor(app.lsp.as_ref(), &loc.file, loc.line, loc.column).await;

    match app.lsp.find_references(&loc.file, line, column).await {
        Ok(locations) => {
            let project_refs: Vec<_> = locations
                .iter()
                .filter(|l| ctx.is_project_path(&l.file))
                .collect();

            let total = project_refs.len();

            let items: Vec<LocationOutput> = project_refs
                .into_iter()
                .take(limit)
                .map(|l| {
                    let mut output =
                        LocationOutput::from_path(&l.file, l.line, l.column, ctx.root());
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

            let hints = refs_hints(&items, total, limit);
            let indexing = app
                .lsp
                .indexing_degradation(crate::models::symbol::Language::from_path(&loc.file))
                .await;
            ctx.print_success(
                Section::with_total(items, total)
                    .with_hints(hints)
                    .with_indexing(indexing),
            );
        }
        Err(e) => ctx.print_error(refs_error(e, &loc.file, line, column)),
    }

    Ok(())
}

/// Enrich the central `LspError → OutputError` mapping with a refs-specific
/// recovery hint when the underlying failure is transport-level
/// (timeout / broken pipe). All other error shapes — including
/// `ServerNotInstalled`, `Unsupported`, parse errors — keep the structured
/// code the central classifier produced.
fn refs_error(
    err: crate::error::LspError,
    file: &std::path::Path,
    line: u32,
    column: u32,
) -> OutputError {
    use crate::cli::ErrorCode;

    let mapped: OutputError = err.into();
    if matches!(mapped.code, ErrorCode::Timeout | ErrorCode::LspUnavailable) {
        return mapped.with_hint(format!(
            "Retry after `symora daemon restart`, or use `symora symbols {0}` and \
             `symora usage {0}:{1}:{2}` to continue from file-level analysis.",
            file.display(),
            line,
            column,
        ));
    }
    mapped
}

fn refs_hints(items: &[LocationOutput], total: usize, limit: usize) -> Vec<String> {
    let mut hints = Vec::new();
    if total == 1 {
        hints.push("Only the declaration reference was found; try `symora usage <location>` for broader symbol-family analysis".to_string());
    }
    if total > limit {
        hints.push("Increase --limit to inspect more references or add --snippet/--context for quicker triage".to_string());
    }
    if items.len() > 1 {
        let unique_files = items
            .iter()
            .map(|item| item.file.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        if unique_files == 1 {
            hints.push("All references are in one file; use `symora context <location> --all` for nearby semantic context".to_string());
        }
    }
    hints.truncate(2);
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_hints_include_self_only_guidance() {
        let items = vec![LocationOutput {
            file: "src/main.rs".to_string(),
            line: 10,
            column: 5,
            snippet: None,
        }];
        let hints = refs_hints(&items, 1, 20);
        assert!(hints.iter().any(|h| h.contains("declaration reference")));
    }
}
