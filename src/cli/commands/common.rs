use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::Section;
use crate::error::LspError;

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
/// Used by implementations, callees, supertypes, subtypes.
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

    match lsp_call(loc.file, loc.line, loc.column).await {
        Ok(items) => {
            let total = items.len();
            let output: Vec<O> = items
                .into_iter()
                .take(limit)
                .map(|item| mapper(item, ctx.root()))
                .collect();
            ctx.print_success(Section::with_limit(output, total));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}
