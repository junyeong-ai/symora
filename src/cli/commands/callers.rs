use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::analysis::{DerivedCallers, callers_from_references, constructions_of};
use crate::cli::declarations_of;
use crate::cli::python_members::{Qualifier, qualifier};
use crate::cli::response::disclosure::{Cap, OutsideCallables, with_cap};
use crate::cli::response::{
    CallHierarchyOutput, NOT_CALLABLE, REFERENCES_DERIVED, Section, TargetOutput,
};
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Location, Symbol};

#[derive(Args, Debug)]
pub struct CallersArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,

    /// Disable fallback to references when call hierarchy unsupported
    #[arg(long, conflicts_with = "from_references")]
    pub no_fallback: bool,

    /// Read callers off the symbol's references, as diff-impact --callers
    /// does, instead of asking the server's call hierarchy
    #[arg(long)]
    pub from_references: bool,
}

/// Callers output. `callers_status` is present only when callers were
/// derived from plain references — because they were asked for
/// (`--from-references`), because the language server lacks call hierarchy, or because a constructor's constructions, which its server
/// reports as references to its type, joined the server's list — those are
/// reference-based callers, a broader approximation rather than verified
/// call edges, and an agent should read them as such — and `not_callable`
/// where the server lacks call hierarchy and the symbol is no function,
/// method or constructor, whose references name no caller. On the exact
/// call-hierarchy path the field is omitted, leaving the bare `Section`
/// contract untouched.
///
/// `target` echoes what the position resolved to, as `refs`, `impact` and
/// `context` do, from the same anchor.
#[derive(Debug, Serialize)]
struct CallersOutput {
    target: TargetOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    callers_status: Option<&'static str>,
    #[serde(flatten)]
    section: Section<CallHierarchyOutput>,
}

pub async fn execute(args: CallersArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.calls_limit);
    let loc = args.loc.parse()?.resolve(None)?;
    let anchor = match crate::cli::commands::common::anchor_of(
        app,
        &loc,
        FindSymbolsOptions::default().with_depth(10),
    )
    .await
    {
        Ok(anchor) => anchor,
        Err(e) => {
            ctx.print_error(e);
            return Ok(());
        }
    };
    let (line, column) = (anchor.line, anchor.column);
    let constructions = match &anchor.symbol {
        Some(symbol) => constructions_of(app.lsp.as_ref(), &anchor.file, symbol, ctx.root()).await,
        None => Ok(None),
    };
    let constructions = match constructions {
        Ok(constructions) => constructions,
        Err(e) => {
            ctx.print_error(e);
            return Ok(());
        }
    };

    // The references callers were read off that named none.
    let mut outside = OutsideCallables::default();
    let hierarchy = match args.from_references {
        true => None,
        false => Some(app.lsp.incoming_calls(&anchor.file, line, column).await),
    };
    let (mut callers, indexing, mut derived) = match hierarchy {
        Some(Ok(calls)) => (calls.data, calls.indexing, false),
        // A transient error surfaces as an error, never as a silently
        // weaker answer.
        Some(Err(e)) if args.no_fallback || !e.is_unsupported() => {
            ctx.print_error(e);
            return Ok(());
        }
        // Asked for, or a capability gap — declared statically or
        // answered as a runtime JSON-RPC MethodNotFound: callers read off
        // the references.
        None | Some(Err(_)) => {
            // A type's or a value's references are annotations, imports
            // and reads rather than calls, so none is read as a caller.
            if let Some(symbol) = anchor.symbol.as_ref().filter(|s| !s.kind.is_callable()) {
                let at = format!(
                    "{}:{}:{}",
                    ctx.relative_path(&anchor.file),
                    anchor.line,
                    anchor.column
                );
                let mut hints = vec![not_callable_hint(symbol, &at)];
                hints.extend(anchor.inbound_hints(ctx.root(), "callers"));
                ctx.print_success(CallersOutput {
                    target: TargetOutput::from_anchor(&anchor, None, ctx.root()),
                    section: Section::with_total(Vec::new(), 0).with_hints(hints),
                    callers_status: Some(NOT_CALLABLE),
                });
                return Ok(());
            }
            match fallback_from_refs(app, &anchor.file, line, column).await {
                Ok((read, indexing)) => {
                    outside = read.outside;
                    (read.callers, indexing, true)
                }
                Err(e) => {
                    ctx.print_error(e);
                    return Ok(());
                }
            }
        }
    };

    let member = qualifier(app, &anchor.file);
    let mut hints: Vec<String> = member.iter().map(|q| q.hint.clone()).collect();
    if let (Some(mut constructions), Some(symbol)) = (constructions, &anchor.symbol) {
        let found = match &constructions.sites {
            Some(sites) => {
                let declarations =
                    declarations_of(app, sites.iter().map(|site| site.file.clone())).await;
                callers_from_references(sites, |file| {
                    declarations.get(file).and_then(|read| read.as_deref().ok())
                })
            }
            None => None,
        };
        match found {
            Some(found) => {
                outside = outside.and(found.outside);
                for caller in found.callers {
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
    hints.extend(outside.hint());
    hints.extend(anchor.inbound_hints(ctx.root(), "callers"));

    let total = callers.len();
    let items: Vec<CallHierarchyOutput> = callers
        .iter()
        .take(limit)
        .map(|c| CallHierarchyOutput::caller(c, ctx.root()))
        .collect();
    ctx.print_success(CallersOutput {
        target: TargetOutput::from_anchor(&anchor, None, ctx.root()),
        section: with_cap(
            Section::with_total(items, total)
                .with_hints(hints)
                .with_indexing(indexing)
                .with_coverage_gaps(member.iter().map(Qualifier::coverage_gap).collect()),
            Cap::Option("--limit"),
        ),
        callers_status: derived.then_some(REFERENCES_DERIVED),
    });
    Ok(())
}

/// Why a symbol that is no function, method or constructor names no
/// caller, its kind in parentheses so no article has to agree with it. `at`
/// is its position, where its uses are asked for.
fn not_callable_hint(symbol: &Symbol, at: &str) -> String {
    format!(
        "`{}` ({}) is no function, method or constructor, and its server has no call \
         hierarchy: its references are uses rather than calls, so none is read as a caller. \
         Its uses: symora refs -- {}",
        symbol.name,
        symbol.kind,
        crate::cli::utils::shell_arg(at)
    )
}

/// Derive callers from plain references when the server lacks call
/// hierarchy, as `diff-impact --callers` derives them
/// ([`callers_from_references`]): every caller, one per callable however
/// many of its references it holds, with how many references named none,
/// and the indexing state the reference query ran under, so the fallback's
/// marker is as computation-time-accurate as the direct path's.
async fn fallback_from_refs(
    app: &App,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<
    (
        DerivedCallers,
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

    /// Kinds begin with vowels and consonants alike (`impl`, `object`,
    /// `class`), so the hint names one without an article to agree with it.
    #[test]
    fn a_not_callable_hint_needs_no_article_and_names_a_command_that_runs() {
        for kind in [
            crate::models::symbol::SymbolKind::Impl,
            crate::models::symbol::SymbolKind::Object,
            crate::models::symbol::SymbolKind::Class,
        ] {
            let symbol = Symbol::new(
                "Shape".to_string(),
                kind,
                Location::point(PathBuf::from("/p/a.py"), 1, 7),
            );
            let hint = not_callable_hint(&symbol, "a.py:1:7");
            assert!(
                hint.starts_with(&format!("`Shape` ({kind}) is no function")),
                "{hint}"
            );
            assert!(
                !hint.contains(" a impl") && !hint.contains(" a object"),
                "{hint}"
            );
            assert!(hint.ends_with("symora refs -- 'a.py:1:7'"), "{hint}");
            crate::cli::assert_named_commands_parse(&hint);
        }
    }

    fn target() -> TargetOutput {
        TargetOutput::new(
            "process".to_string(),
            "function".to_string(),
            "a.py".to_string(),
            1,
            5,
        )
    }

    /// The echoed target leads, ahead of the list's qualifiers and items.
    #[test]
    fn the_target_leads_the_answer() {
        let output = CallersOutput {
            target: target(),
            section: Section::with_total(Vec::<CallHierarchyOutput>::new(), 0),
            callers_status: None,
        };
        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["target"]["name"], "process");
        assert_eq!(value.as_object().unwrap().keys().next().unwrap(), "target");
    }

    #[test]
    fn exact_call_hierarchy_omits_callers_status() {
        let output = CallersOutput {
            target: target(),
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
            target: target(),
            section: Section::with_total(Vec::<CallHierarchyOutput>::new(), 0),
            callers_status: Some(REFERENCES_DERIVED),
        };
        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["callers_status"], "references_derived");
    }
}
