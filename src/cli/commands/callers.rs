use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::analysis::callers_from_references;
use crate::cli::response::{CallHierarchyOutput, REFERENCES_DERIVED, Section};
use crate::error::LspError;
use crate::models::lsp::{CallHierarchyItem, FindSymbolsOptions};
use crate::models::symbol::{Location, Symbol};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    callers_status: Option<&'static str>,
    #[serde(flatten)]
    section: Section<CallHierarchyOutput>,
}

pub async fn execute(args: CallersArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.calls_limit);
    let loc = args.loc.parse()?.to_absolute()?;
    let anchor = crate::cli::commands::common::anchor_of(
        app,
        &loc,
        FindSymbolsOptions::default().with_depth(10),
    )
    .await;
    let (line, column) = (anchor.line, anchor.column);

    let result = app.lsp.incoming_calls(&anchor.file, line, column).await;

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
                    .with_hints(anchor.inbound_hints(ctx.root(), "callers"))
                    .with_indexing(calls.indexing),
                callers_status: None,
            });
        }
        // A capability gap — declared statically or answered as a runtime
        // JSON-RPC MethodNotFound — falls back to references-derived callers;
        // transient errors surface as errors, never as a silently weaker
        // answer.
        Err(ref e) if !args.no_fallback && e.is_unsupported() => {
            match fallback_from_refs(app, &anchor.file, line, column, limit).await {
                Ok((calls, total_refs, indexing)) => {
                    let items: Vec<CallHierarchyOutput> = calls
                        .iter()
                        .map(|c| CallHierarchyOutput::from_item(c, ctx.root()))
                        .collect();

                    ctx.print_success(CallersOutput {
                        section: Section::with_total(items, total_refs)
                            .with_hints(anchor.inbound_hints(ctx.root(), "callers"))
                            .with_indexing(indexing),
                        callers_status: Some(REFERENCES_DERIVED),
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
/// hierarchy, as `diff-impact --callers` derives them
/// ([`callers_from_references`]). Returns up to `limit` caller items plus
/// the exact count of *unique callers* found — the same domain as the
/// items, never the raw reference count (several references inside one
/// caller are one caller) — and the indexing state the reference query ran
/// under, so the fallback's marker is as computation-time-accurate as the
/// direct path's.
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
    let references: Vec<Location> = refs
        .data
        .into_iter()
        .filter(|reference| !reference.covers(file, line, column))
        .collect();

    let mut symbols: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
    for reference in &references {
        if !symbols.contains_key(&reference.file) {
            let fetched = app
                .lsp
                .find_symbols(
                    &reference.file,
                    FindSymbolsOptions::default().with_depth(10),
                )
                .await?;
            symbols.insert(reference.file.clone(), fetched);
        }
    }
    let mut callers =
        callers_from_references(&references, |file| symbols.get(file).map(Vec::as_slice))
            .expect("every referencing file's symbols were read");
    let total = callers.len();
    callers.truncate(limit);
    Ok((callers, total, indexing))
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
            callers_status: Some(REFERENCES_DERIVED),
        };
        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["callers_status"], "references_derived");
    }
}
