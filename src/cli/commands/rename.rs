use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::commands::edit::apply_workspace_edits;
#[cfg(unix)]
use crate::cli::commands::edit::invalidate_store_files;
use crate::cli::response::FileChangeOutput;
use crate::cli::utils::find_symbol_at_position;
use crate::models::lsp::FindSymbolsOptions;

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
        };
        ctx.print_success(response);
        return Ok(());
    }

    match app
        .lsp
        .rename(&loc.file, loc.line, loc.column, &args.new_name)
        .await
    {
        Ok(result) => {
            // Apply workspace edits to files
            match apply_workspace_edits(&result.changes, args.dry_run) {
                Ok(applied_changes) => {
                    #[cfg(unix)]
                    if !args.dry_run {
                        let changed_files: Vec<_> =
                            applied_changes.iter().map(|c| c.file.clone()).collect();
                        invalidate_store_files(app, &changed_files).await;
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
                    };
                    ctx.print_success(response);
                }
                Err(e) => ctx.print_error(e),
            }
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
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

    // 2. Try find_symbols and match by position
    if let Ok(symbols) = app
        .lsp
        .find_symbols(&loc.file, FindSymbolsOptions::default().with_depth(10))
        .await
        && let Some(sym) = find_symbol_at_position(&symbols, loc.line, Some(loc.column))
    {
        return Some(sym.name.clone());
    }

    // 3. Fall back to hover (least reliable)
    app.lsp
        .hover(&loc.file, loc.line, loc.column)
        .await
        .ok()
        .flatten()
        .and_then(|h| h.extract_symbol_name())
        .filter(|s| !s.is_empty())
}
