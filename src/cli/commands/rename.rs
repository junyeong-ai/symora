use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::commands::edit::apply_workspace_edits;
use crate::cli::commands::edit::refresh_store_files;
use crate::cli::response::FileChangeOutput;
use crate::cli::utils::find_named_at_position;
use crate::cli::{OutputError, ParsedLocation};
use crate::models::lsp::{FindSymbolsOptions, IndexingDegradation};

#[derive(Args, Debug)]
pub struct RenameArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// New name for the symbol
    pub new_name: String,

    /// Preview changes without applying
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Serialize)]
struct RenameOutput {
    old_name: Option<String>,
    new_name: String,
    dry_run: bool,
    affected_files: usize,
    changes: Vec<FileChangeOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    indexing: Option<IndexingDegradation>,
}

pub async fn execute(args: RenameArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute_with_root(Some(app.root()))?;

    if let Err(e) = loc.validate_position_async().await {
        ctx.print_error(e);
        return Ok(());
    }

    // Strategy: prepareRename > find_symbols > hover
    let old_name = get_symbol_name_at_position(app, &loc).await;

    if let Some(ref current) = old_name
        && current == &args.new_name
    {
        let response = RenameOutput {
            old_name: old_name.clone(),
            new_name: args.new_name,
            dry_run: true,
            affected_files: 0,
            changes: vec![],
            message: Some("Symbol is already named the same. No changes needed.".to_string()),
            indexing: None,
        };
        ctx.print_success(response);
        return Ok(());
    }

    let answer = match app
        .lsp
        .rename(&loc.file, loc.line, loc.column, &args.new_name)
        .await
    {
        Ok(answer) => answer,
        Err(e) => {
            ctx.print_error(e);
            return Ok(());
        }
    };

    if let Some(refusal) = refuse_partial_rename(answer.indexing, args.dry_run) {
        ctx.print_error(refusal);
        return Ok(());
    }

    match answer.data {
        None => ctx.print_error(
            OutputError::invalid(format!(
                "No renameable symbol at {}:{}:{}",
                ctx.relative_path(&loc.file),
                loc.line,
                loc.column
            ))
            .with_hint(
                "Point at an identifier — `symora hover` or `symora def` at the position shows \
                 what the language server reads there, and `symora symbols <file>` lists the \
                 declarations to target",
            ),
        ),
        Some(result) => {
            // Apply workspace edits to files
            match apply_workspace_edits(&result.changes, args.dry_run, app.root()) {
                Ok(applied_changes) => {
                    if !args.dry_run {
                        let changed_files: Vec<_> =
                            applied_changes.iter().map(|c| c.file.clone()).collect();
                        refresh_store_files(app, &changed_files).await;
                    }

                    let changes: Vec<FileChangeOutput> = applied_changes
                        .iter()
                        .map(|fc| FileChangeOutput {
                            file: ctx.relative_path(&fc.file),
                            edit_count: fc.edit_count,
                        })
                        .collect();

                    let response = RenameOutput {
                        old_name,
                        new_name: args.new_name,
                        dry_run: args.dry_run,
                        affected_files: changes.len(),
                        changes,
                        message: if args.dry_run {
                            Some(
                                "Preview only. Run without --dry-run to apply changes.".to_string(),
                            )
                        } else {
                            None
                        },
                        indexing: answer.indexing,
                    };
                    ctx.print_success(response);
                }
                Err(e) => ctx.print_error(e),
            }
        }
    }

    Ok(())
}

/// A rename is the language server's reference set turned into edits, so a
/// set computed while the workspace was still indexing renames some call
/// sites and leaves the rest — a broken tree the disclosure cannot undo.
/// A preview still runs: it shows what has been computed so far, marked as
/// the lower bound it is.
fn refuse_partial_rename(
    indexing: Option<IndexingDegradation>,
    dry_run: bool,
) -> Option<OutputError> {
    let degradation = indexing.filter(|_| !dry_run)?;
    Some(
        OutputError::precondition_failed(format!(
            "Rename refused: the edit set is a lower bound under degraded indexing \
             (indexing: {})",
            degradation.as_str(),
        ))
        .with_hint(
            "Wait for the language server to finish indexing (check `symora status`), \
             then retry; `--dry-run` previews the edits computed so far.",
        ),
    )
}

async fn get_symbol_name_at_position(app: &App, loc: &ParsedLocation) -> Option<String> {
    // 1. Try prepareRename (LSP standard)
    if let Ok(Some(result)) = app
        .lsp
        .prepare_rename(&loc.file, loc.line, loc.column)
        .await
    {
        return Some(result.placeholder);
    }

    // 2. Try find_symbols: the symbol whose name the position is on
    if let Ok(symbols) = app
        .lsp
        .find_symbols(&loc.file, FindSymbolsOptions::default().with_depth(10))
        .await
        && let Some(sym) = find_named_at_position(&symbols, loc.line, loc.column)
    {
        return Some(sym.name.clone());
    }

    // 3. Fall back to hover (least reliable)
    app.lsp
        .hover(&loc.file, loc.line, loc.column)
        .await
        .ok()
        .and_then(|hover| hover.data)
        .and_then(|h| h.extract_symbol_name())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::errors::ErrorCode;

    #[test]
    fn a_rename_computed_under_degraded_indexing_applies_nothing() {
        let degraded = Some(IndexingDegradation::TimedOut);
        assert_eq!(
            refuse_partial_rename(degraded, false).map(|e| e.code),
            Some(ErrorCode::PreconditionFailed)
        );
        assert!(refuse_partial_rename(degraded, true).is_none());
        assert!(refuse_partial_rename(None, false).is_none());
    }
}
