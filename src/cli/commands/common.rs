use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::Section;
use crate::cli::utils::{SymbolResolution, column_addressed_symbol, line_addressed_symbol};
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::services::lsp::LspService;

/// A snapped anchor position plus its disclosure: when a line-only input
/// hit a multi-declaration line, the first declaration was chosen and
/// `hint` names the alternatives.
pub(crate) struct SnappedAnchor {
    pub line: u32,
    pub column: u32,
    pub hint: Option<String>,
}

/// Snap an input position to the authoritative name anchor of the symbol
/// it addresses, through the same line/column addressing rules the edit
/// surface uses (`cli::utils::symbol_nav`): an explicit column resolves
/// position-precisely (with the declaration-start fallback that makes a
/// `search symbols` row's `pub`/`fn` column work); an omitted column
/// addresses the symbol DECLARED on the line, body lines falling back to
/// the enclosing symbol. Symbol-level commands — references, callers,
/// callees, implementations, type hierarchy, impact, context — all route
/// through these rules so "the symbol on this line" means the same thing
/// on every surface. Position-exact commands (def, hover, typedef)
/// deliberately do not.
///
/// Ambiguity (several declarations on one line, no column) resolves to
/// the line's first declaration with a disclosure hint — navigation
/// discloses where an edit would refuse.
pub(crate) async fn snap_to_symbol_anchor(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: Option<u32>,
) -> SnappedAnchor {
    let unsnapped = SnappedAnchor {
        line,
        column: column.unwrap_or(1),
        hint: None,
    };
    let Ok(symbols) = lsp
        .find_symbols(file, FindSymbolsOptions::default().with_depth(10))
        .await
    else {
        return unsnapped;
    };
    let resolution = match column {
        Some(column) => column_addressed_symbol(&symbols, line, column),
        None => line_addressed_symbol(&symbols, line),
    };
    match resolution {
        SymbolResolution::Match(symbol) => SnappedAnchor {
            line: symbol.location.line,
            column: symbol.location.column,
            hint: None,
        },
        SymbolResolution::Ambiguous(declared) => {
            let names: Vec<&str> = declared.iter().map(|s| s.name.as_str()).collect();
            let first = declared[0];
            SnappedAnchor {
                line: first.location.line,
                column: first.location.column,
                hint: Some(format!(
                    "Line {} declares multiple symbols ({}); resolved to '{}' — pass an \
                     explicit column (file:line:column) to target another",
                    line,
                    names.join(", "),
                    first.name,
                )),
            }
        }
        SymbolResolution::NotFound => unsnapped,
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

/// Execute a command that returns `Indexed<Vec<T>>` from an LSP call,
/// wrapping in `Section`. Used by implementations, callees, supertypes,
/// subtypes — all cross-file graph queries, so each carries the
/// workspace-indexing degradation marker captured when the query ran.
pub async fn execute_list<T, O, F, Fut, M>(
    app: &App,
    loc: LocationArg,
    limit: usize,
    lsp_call: F,
    mapper: M,
) -> Result<()>
where
    F: FnOnce(PathBuf, u32, u32) -> Fut,
    Fut: Future<Output = Result<crate::models::lsp::Indexed<Vec<T>>, LspError>>,
    M: Fn(T, &Path) -> O,
    O: Serialize,
{
    let ctx = &app.output;
    let loc = loc.parse()?.to_absolute()?;
    let anchor = snap_to_symbol_anchor(
        app.lsp.as_ref(),
        &loc.file,
        loc.line,
        loc.column_explicit.then_some(loc.column),
    )
    .await;

    match lsp_call(loc.file, anchor.line, anchor.column).await {
        Ok(result) => {
            let total = result.data.len();
            let output: Vec<O> = result
                .data
                .into_iter()
                .take(limit)
                .map(|item| mapper(item, ctx.root()))
                .collect();
            let hints = anchor.hint.map(|h| vec![h]).unwrap_or_default();
            ctx.print_success(
                Section::with_total(output, total)
                    .with_hints(hints)
                    .with_indexing(result.indexing),
            );
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
