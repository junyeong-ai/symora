use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::LocationOutput;
use crate::cli::utils::{read_line_at, read_lines_around, resolve_symbol_anchor};
use crate::models::lsp::FindSymbolsOptions;

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

#[derive(Serialize)]
struct RefsOutput {
    count: usize,
    showing: usize,
    items: Vec<LocationOutput>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hints: Vec<String>,
}

pub async fn execute(args: RefsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.refs_limit);
    let loc = args.loc.parse()?.to_absolute_with_root(Some(app.root()))?;

    let (line, column) = match app
        .lsp
        .find_symbols(&loc.file, FindSymbolsOptions::default().with_depth(10))
        .await
    {
        Ok(symbols) => resolve_symbol_anchor(&symbols, loc.line, loc.column)
            .map(|(line, column, _)| (line, column))
            .unwrap_or((loc.line, loc.column)),
        Err(_) => (loc.line, loc.column),
    };

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

            ctx.print_success(RefsOutput {
                count: total,
                showing: items.len(),
                truncated: total > limit,
                hints: refs_hints(&items, total, limit),
                items,
            });
        }
        Err(e) => ctx.print_error(&format_refs_error(&e.to_string(), &loc.file, line, column)),
    }

    Ok(())
}

fn format_refs_error(error: &str, file: &std::path::Path, line: u32, column: u32) -> String {
    if error.contains("Broken pipe") || error.contains("timed out") || error.contains("timeout") {
        format!(
            "The language server did not respond cleanly for references here. Retry after `symora daemon restart`, or use `symora symbols {}` and `symora usage {}:{}:{}` to continue from file-level analysis.",
            file.display(),
            file.display(),
            line,
            column
        )
    } else {
        error.to_string()
    }
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
    fn refs_output_uses_items_and_showing() {
        let output = RefsOutput {
            count: 4,
            showing: 2,
            items: vec![LocationOutput {
                file: "src/main.rs".to_string(),
                line: 10,
                column: 5,
                snippet: None,
            }],
            truncated: true,
            hints: vec!["Increase --limit".to_string()],
        };

        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["count"], 4);
        assert_eq!(value["showing"], 2);
        assert!(value.get("items").is_some());
        assert_eq!(value["truncated"], true);
    }

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
