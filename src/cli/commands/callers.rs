use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::analysis::{callers_from_references, constructions_of};
use crate::cli::declarations_of;
use crate::cli::response::{CallHierarchyOutput, NOT_CALLABLE, REFERENCES_DERIVED, Section};
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

/// Callers output. `callers_status` is present only when callers were
/// derived from plain references — because the language server lacks call
/// hierarchy, or because a constructor's constructions, which its server
/// reports as references to its type, joined the server's list — those are
/// reference-based callers, a broader approximation rather than verified
/// call edges, and an agent should read them as such — and `not_callable`
/// where the server lacks call hierarchy and the symbol is no function,
/// method or constructor, whose references name no caller. On the exact
/// call-hierarchy path the field is omitted, leaving the bare `Section`
/// contract untouched.
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
    let constructions = match &anchor.symbol {
        Some(symbol) => constructions_of(app.lsp.as_ref(), &anchor.file, symbol, ctx.root()).await,
        None => None,
    };

    let (mut callers, indexing, mut derived) =
        match app.lsp.incoming_calls(&anchor.file, line, column).await {
            Ok(calls) => (calls.data, calls.indexing, false),
            // A capability gap — declared statically or answered as a runtime
            // JSON-RPC MethodNotFound — falls back to references-derived
            // callers; transient errors surface as errors, never as a
            // silently weaker answer.
            Err(ref e) if !args.no_fallback && e.is_unsupported() => {
                // A type's or a value's references are annotations, imports
                // and reads rather than calls, so none is read as a caller.
                if let Some(symbol) = anchor.symbol.as_ref().filter(|s| !s.kind.is_callable()) {
                    let mut hints = vec![format!(
                        "`{}` is a {}, no function, method or constructor, and its server has \
                         no call hierarchy: its references are uses rather than calls, so none \
                         is read as a caller; `symora refs` lists them",
                        symbol.name, symbol.kind
                    )];
                    hints.extend(anchor.inbound_hints(ctx.root(), "callers"));
                    ctx.print_success(CallersOutput {
                        section: Section::with_total(Vec::new(), 0).with_hints(hints),
                        callers_status: Some(NOT_CALLABLE),
                    });
                    return Ok(());
                }
                match fallback_from_refs(app, &anchor.file, line, column).await {
                    Ok((calls, indexing)) => (calls, indexing, true),
                    Err(e) => {
                        ctx.print_error(e);
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                ctx.print_error(e);
                return Ok(());
            }
        };

    let mut hints = Vec::new();
    if let (Some(mut constructions), Some(symbol)) = (constructions, &anchor.symbol) {
        let found = match &constructions.sites {
            Some(sites) => {
                let declarations =
                    declarations_of(app, sites.iter().map(|site| site.file.clone())).await;
                callers_from_references(sites, |file| {
                    declarations.get(file).and_then(|read| read.as_deref())
                })
            }
            None => None,
        };
        match found {
            Some(found) => {
                for caller in found {
                    if !callers
                        .iter()
                        .any(|named| named.location == caller.location)
                    {
                        callers.push(caller);
                        derived = true;
                    }
                }
            }
            // A construction whose file no reader could read names no
            // caller anyone can stand behind.
            None => constructions.sites = None,
        }
        hints.push(constructions.hint(&symbol.name));
    }
    hints.extend(anchor.inbound_hints(ctx.root(), "callers"));

    let total = callers.len();
    let items: Vec<CallHierarchyOutput> = callers
        .iter()
        .take(limit)
        .map(|c| CallHierarchyOutput::from_item(c, ctx.root()))
        .collect();
    ctx.print_success(CallersOutput {
        section: Section::with_total(items, total)
            .with_hints(hints)
            .with_indexing(indexing),
        callers_status: derived.then_some(REFERENCES_DERIVED),
    });
    Ok(())
}

/// Derive callers from plain references when the server lacks call
/// hierarchy, as `diff-impact --callers` derives them
/// ([`callers_from_references`]): every caller, one per callable however
/// many of its references it holds, and the indexing state the reference
/// query ran under, so the fallback's marker is as computation-time-accurate
/// as the direct path's.
async fn fallback_from_refs(
    app: &App,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<
    (
        Vec<CallHierarchyItem>,
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
    let callers = callers_from_references(&references, |file| symbols.get(file).map(Vec::as_slice))
        .expect("every referencing file's symbols were read");
    Ok((callers, indexing))
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
