use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::{CallHierarchyOutput, Section};
use crate::error::LspError;
use crate::models::lsp::{CallHierarchyItem, FindSymbolsOptions};
use crate::services::lsp::find_containing_callable;

#[derive(Args, Debug)]
pub struct CallersArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,

    /// Disable fallback to references when call hierarchy unsupported
    #[arg(long)]
    pub no_fallback: bool,
}

pub async fn execute(args: CallersArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.calls_limit);
    let loc = args.loc.parse()?.to_absolute()?;

    let result = app
        .lsp
        .incoming_calls(&loc.file, loc.line, loc.column)
        .await;

    match result {
        Ok(calls) => {
            let total = calls.len();
            let items: Vec<CallHierarchyOutput> = calls
                .into_iter()
                .take(limit)
                .map(|c| CallHierarchyOutput::from_item(&c, ctx.root()))
                .collect();

            ctx.print_success(Section::with_limit(items, total));
        }
        Err(ref e) if !args.no_fallback && is_not_supported(e) => {
            match fallback_from_refs(app, &loc.file, loc.line, loc.column, limit).await {
                Ok((calls, total_refs)) => {
                    let items: Vec<CallHierarchyOutput> = calls
                        .iter()
                        .map(|c| CallHierarchyOutput::from_item(c, ctx.root()))
                        .collect();

                    ctx.print_success(Section::with_limit(items, total_refs));
                }
                Err(e) => ctx.print_error(e),
            }
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

fn is_not_supported(err: &LspError) -> bool {
    matches!(err, LspError::FeatureNotSupported { .. })
}

async fn fallback_from_refs(
    app: &App,
    file: &Path,
    line: u32,
    column: u32,
    limit: usize,
) -> Result<(Vec<CallHierarchyItem>, usize), LspError> {
    let refs = app.lsp.find_references(file, line, column).await?;
    let total_refs = refs.len();

    let mut seen = HashSet::new();
    let mut callers = Vec::new();
    let mut symbol_cache: HashMap<PathBuf, Vec<crate::models::symbol::Symbol>> = HashMap::new();

    for ref_loc in refs {
        if ref_loc.file == file && ref_loc.line == line {
            continue;
        }

        let symbols = match symbol_cache.get(&ref_loc.file) {
            Some(cached) => cached,
            None => {
                let fetched = app
                    .lsp
                    .find_symbols(&ref_loc.file, FindSymbolsOptions::default().with_depth(10))
                    .await?;
                symbol_cache.entry(ref_loc.file.clone()).or_insert(fetched)
            }
        };

        if let Some(caller) = find_containing_callable(symbols, ref_loc.line) {
            let key = (
                caller.name.clone(),
                caller.location.file.clone(),
                caller.location.line,
            );

            if seen.insert(key) {
                callers.push(CallHierarchyItem {
                    name: caller.name.clone(),
                    kind: caller.kind,
                    location: caller.location.clone(),
                    call_site: Some(ref_loc),
                });

                if callers.len() >= limit {
                    break;
                }
            }
        }
    }

    Ok((callers, total_refs))
}
