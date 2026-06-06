use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::Section;
use crate::cli::utils::resolve_symbol_anchor;
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::services::lsp::LspService;

/// Snap an input position to the authoritative name anchor of the symbol it
/// falls in, so a line-only or declaration-start position (the column
/// `search symbols` reports, e.g. the `pub`/`fn` keyword) resolves the same
/// way an exact name position would. Symbol-level commands — references,
/// callers, callees, implementations, type hierarchy, impact, context — all
/// route through this so they agree on what "the symbol here" means.
/// Position-exact commands (def, hover, typedef) deliberately do not.
pub(crate) async fn snap_to_symbol_anchor(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
) -> (u32, u32) {
    match lsp
        .find_symbols(file, FindSymbolsOptions::default().with_depth(10))
        .await
    {
        Ok(symbols) => resolve_symbol_anchor(&symbols, line, column)
            .map(|(line, column, _)| (line, column))
            .unwrap_or((line, column)),
        Err(_) => (line, column),
    }
}

/// Execute a command that returns `Option<T>` from an LSP call.
/// Used by def, typedef, hover, signature.
pub async fn execute_optional<T, O, F, Fut, M, N>(
    app: &App,
    loc: LocationArg,
    lsp_call: F,
    on_found: M,
    on_not_found: N,
) -> Result<()>
where
    F: FnOnce(PathBuf, u32, u32) -> Fut,
    Fut: Future<Output = Result<Option<T>, LspError>>,
    M: FnOnce(T, &crate::cli::output::OutputContext) -> O,
    N: FnOnce() -> O,
    O: Serialize,
{
    let ctx = &app.output;
    let loc = loc.parse()?.to_absolute()?;

    match lsp_call(loc.file, loc.line, loc.column).await {
        Ok(Some(result)) => ctx.print_success(on_found(result, ctx)),
        Ok(None) => ctx.print_success(on_not_found()),
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

/// Execute a command that returns `Vec<T>` from an LSP call, wrapping in `Section`.
/// Used by implementations, callees, supertypes, subtypes — all cross-file
/// graph queries, so each carries the workspace-indexing degradation
/// marker when the answer was computed on a cold server.
pub async fn execute_list<T, O, F, Fut, M>(
    app: &App,
    loc: LocationArg,
    limit: usize,
    lsp_call: F,
    mapper: M,
) -> Result<()>
where
    F: FnOnce(PathBuf, u32, u32) -> Fut,
    Fut: Future<Output = Result<Vec<T>, LspError>>,
    M: Fn(T, &Path) -> O,
    O: Serialize,
{
    let ctx = &app.output;
    let loc = loc.parse()?.to_absolute()?;
    let language = crate::models::symbol::Language::from_path(&loc.file);
    let (line, column) =
        snap_to_symbol_anchor(app.lsp.as_ref(), &loc.file, loc.line, loc.column).await;

    match lsp_call(loc.file, line, column).await {
        Ok(items) => {
            let total = items.len();
            let output: Vec<O> = items
                .into_iter()
                .take(limit)
                .map(|item| mapper(item, ctx.root()))
                .collect();
            let indexing = app.lsp.indexing_degradation(language).await;
            ctx.print_success(Section::with_total(output, total).with_indexing(indexing));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
