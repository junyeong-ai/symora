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

    /// Apply although the language server cannot confirm workspace readiness
    /// (`indexing: unconfirmed`), after reviewing a `--dry-run`; the response
    /// keeps the marker. A readiness wait that timed out is still refused.
    #[arg(long)]
    pub accept_unconfirmed: bool,
}

#[derive(Serialize)]
struct RenameOutput {
    old_name: Option<String>,
    new_name: String,
    dry_run: bool,
    affected_files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    indexing: Option<IndexingDegradation>,
    changes: Vec<FileChangeOutput>,
}

pub async fn execute(args: RenameArgs, app: &App) -> Result<()> {
    let _lock = (!args.dry_run)
        .then(|| super::edit::lock_edits(app.root()))
        .transpose()?;
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute_with_root(Some(app.root()))?;

    if let Err(e) = loc.validate_position_async().await {
        ctx.print_error(e);
        return Ok(());
    }

    let old_name = get_symbol_name_at_position(app, &loc).await;

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

    if let Some(refusal) =
        refuse_partial_rename(answer.indexing, args.dry_run, args.accept_unconfirmed)
    {
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
                        message: rename_message(args.dry_run, answer.indexing).map(str::to_string),
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
///
/// The two degradations differ in what can settle them. A timed-out wait is
/// one a retry can still see confirmed, so it is never applied. An
/// unconfirmed one is usually a server with no readiness channel at all,
/// where no retry ever confirms, so the caller may accept it once they have
/// reviewed the preview.
fn refuse_partial_rename(
    indexing: Option<IndexingDegradation>,
    dry_run: bool,
    accept_unconfirmed: bool,
) -> Option<OutputError> {
    let degradation = indexing.filter(|_| !dry_run)?;
    let hint = match degradation {
        IndexingDegradation::TimedOut => {
            "Use `--dry-run` to inspect the edits. The readiness wait exhausted its budget; \
             retrying may help when the language server subsequently confirms readiness."
        }
        IndexingDegradation::Unconfirmed if accept_unconfirmed => return None,
        IndexingDegradation::Unconfirmed => {
            "Use `--dry-run` to inspect the edits, then apply them with `--accept-unconfirmed`. \
             The language server has not confirmed workspace readiness; retrying alone does \
             not establish completeness."
        }
    };
    Some(
        OutputError::precondition_failed(format!(
            "Rename refused: workspace completeness is not confirmed for the edit set \
             (indexing: {})",
            degradation.as_str(),
        ))
        .with_hint(hint),
    )
}

/// What a rename that went through says about itself: a preview names how
/// its edits can be applied, and an application the server could not confirm
/// names what is left to check.
fn rename_message(dry_run: bool, indexing: Option<IndexingDegradation>) -> Option<&'static str> {
    match (dry_run, indexing) {
        (true, None) => Some("Preview only. Run without --dry-run to apply changes."),
        (true, Some(IndexingDegradation::Unconfirmed)) => Some(
            "Preview only. Workspace readiness was not confirmed; review the edits, then apply \
             them with --accept-unconfirmed.",
        ),
        (true, Some(IndexingDegradation::TimedOut)) => Some(
            "Preview only. The readiness wait timed out; application is refused until a retry \
             confirms readiness.",
        ),
        (false, Some(_)) => Some(
            "Applied without confirmed workspace readiness; check that no reference kept the \
             old name.",
        ),
        (false, None) => None,
    }
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

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::errors::ErrorCode;

    #[test]
    fn unverified_rename_edits_are_preview_only_unless_an_unconfirmed_one_is_accepted() {
        for degradation in [
            IndexingDegradation::TimedOut,
            IndexingDegradation::Unconfirmed,
        ] {
            let error = refuse_partial_rename(Some(degradation), false, false).unwrap();
            assert_eq!(error.code, ErrorCode::PreconditionFailed);
            assert!(error.message.contains(degradation.as_str()));
            assert!(refuse_partial_rename(Some(degradation), true, false).is_none());
        }
        assert!(
            refuse_partial_rename(Some(IndexingDegradation::Unconfirmed), false, true).is_none()
        );
        assert!(
            refuse_partial_rename(Some(IndexingDegradation::TimedOut), false, true).is_some(),
            "a wait a retry can still settle is never accepted"
        );
        assert!(refuse_partial_rename(None, false, false).is_none());
        assert!(refuse_partial_rename(None, true, false).is_none());
    }
}
