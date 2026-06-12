use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde::Serialize;

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

/// Callers output. `callers_status` is present only when the callers were
/// derived from plain references because the language server lacks call
/// hierarchy — those are reference-based callers, a broader approximation
/// rather than verified call edges, and an agent should read them as such.
/// On the exact call-hierarchy path the field is omitted, leaving the bare
/// `Section` contract untouched.
#[derive(Debug, Serialize)]
struct CallersOutput {
    #[serde(flatten)]
    section: Section<CallHierarchyOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    callers_status: Option<&'static str>,
}

pub async fn execute(args: CallersArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.calls_limit);
    let loc = args.loc.parse()?.to_absolute()?;
    let anchor = crate::cli::commands::common::snap_to_symbol_anchor(
        app.lsp.as_ref(),
        &loc.file,
        loc.line,
        loc.column_explicit.then_some(loc.column),
    )
    .await;
    let (line, column) = (anchor.line, anchor.column);
    let anchor_hints = || anchor.hint.clone().map(|h| vec![h]).unwrap_or_default();

    let result = app.lsp.incoming_calls(&loc.file, line, column).await;

    match result {
        Ok(calls) => {
            let total = calls.data.len();
            let items: Vec<CallHierarchyOutput> = calls
                .data
                .into_iter()
                .take(limit)
                .map(|c| CallHierarchyOutput::from_item(&c, ctx.root()))
                .collect();

            ctx.print_success(CallersOutput {
                section: Section::with_total(items, total)
                    .with_hints(anchor_hints())
                    .with_indexing(calls.indexing),
                callers_status: None,
            });
        }
        // A capability gap — declared statically or answered as a runtime
        // JSON-RPC MethodNotFound — falls back to references-derived callers;
        // transient errors surface as errors, never as a silently weaker
        // answer.
        Err(ref e) if !args.no_fallback && e.is_unsupported() => {
            match fallback_from_refs(app, &loc.file, line, column, limit).await {
                Ok((calls, total_refs, indexing)) => {
                    let items: Vec<CallHierarchyOutput> = calls
                        .iter()
                        .map(|c| CallHierarchyOutput::from_item(c, ctx.root()))
                        .collect();

                    ctx.print_success(CallersOutput {
                        section: Section::with_total(items, total_refs)
                            .with_hints(anchor_hints())
                            .with_indexing(indexing),
                        callers_status: Some("references_derived"),
                    });
                }
                Err(e) => ctx.print_error(e),
            }
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

/// Derive callers from plain references when the server lacks call
/// hierarchy. Returns up to `limit` caller items plus the exact count of
/// *unique callers* found — the same domain as the items, never the raw
/// reference count (several references inside one caller are one caller)
/// — and the indexing state the reference query ran under, so the
/// fallback's marker is as computation-time-accurate as the direct path's.
async fn fallback_from_refs(
    app: &App,
    file: &Path,
    line: u32,
    column: u32,
    limit: usize,
) -> Result<
    (
        Vec<CallHierarchyItem>,
        usize,
        Option<crate::models::lsp::IndexingDegradation>,
    ),
    LspError,
> {
    let refs = app.lsp.find_references(file, line, column).await?;
    let indexing = refs.indexing;

    let mut seen = HashSet::new();
    let mut callers = Vec::new();
    let mut symbol_cache: HashMap<PathBuf, Vec<crate::models::symbol::Symbol>> = HashMap::new();

    for ref_loc in refs.data {
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

            // Keep counting unique callers past the emission cap so the
            // reported total stays exact; only storage is capped.
            if seen.insert(key) && callers.len() < limit {
                callers.push(CallHierarchyItem {
                    name: caller.name.clone(),
                    kind: caller.kind,
                    location: caller.location.clone(),
                    call_site: Some(ref_loc),
                });
            }
        }
    }

    Ok((callers, seen.len(), indexing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_call_hierarchy_omits_callers_status() {
        let output = CallersOutput {
            section: Section::with_total(Vec::<CallHierarchyOutput>::new(), 0),
            callers_status: None,
        };
        let value = serde_json::to_value(output).unwrap();
        assert!(value.get("callers_status").is_none());
        assert!(value.get("items").is_some());
    }

    #[test]
    fn references_derived_fallback_marks_callers_status() {
        let output = CallersOutput {
            section: Section::with_total(Vec::<CallHierarchyOutput>::new(), 0),
            callers_status: Some("references_derived"),
        };
        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["callers_status"], "references_derived");
    }
}
