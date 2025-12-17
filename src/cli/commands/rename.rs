//! Rename command - LSP-powered symbol renaming

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Symbol;

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
struct RenameResponse {
    old_name: Option<String>,
    new_name: String,
    dry_run: bool,
    affected_files: usize,
    changes: Vec<FileChangeOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize)]
struct FileChangeOutput {
    file: String,
    edit_count: usize,
}

pub async fn execute(args: RenameArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    if let Err(e) = loc.validate_position_async().await {
        ctx.print_error(&e.to_string());
        return Ok(());
    }

    // Strategy: prepareRename > find_symbols > hover
    let old_name = get_symbol_name_at_position(app, &loc).await;

    if let Some(ref current) = old_name
        && current == &args.new_name
    {
        let response = RenameResponse {
            old_name: old_name.clone(),
            new_name: args.new_name,
            dry_run: true,
            affected_files: 0,
            changes: vec![],
            message: Some("Symbol is already named the same. No changes needed.".to_string()),
        };
        ctx.print_success_flat(response);
        return Ok(());
    }

    match app
        .lsp
        .rename(&loc.file, loc.line, loc.column, &args.new_name)
        .await
    {
        Ok(result) => {
            let changes: Vec<FileChangeOutput> = result
                .changes
                .iter()
                .map(|fc| FileChangeOutput {
                    file: ctx.relative_path(&fc.file),
                    edit_count: fc.edit_count,
                })
                .collect();

            let response = RenameResponse {
                old_name,
                new_name: args.new_name,
                dry_run: args.dry_run,
                affected_files: changes.len(),
                changes,
                message: None,
            };
            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
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
    if let Ok(mut symbols) = app
        .lsp
        .find_symbols(&loc.file, FindSymbolsOptions::new().with_depth(10))
        .await
    {
        Symbol::compute_paths_for_all(&mut symbols);
        if let Some(name) = find_symbol_at_position(&symbols, loc.line, loc.column) {
            return Some(name);
        }
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

fn find_symbol_at_position(symbols: &[Symbol], line: u32, column: u32) -> Option<String> {
    for symbol in symbols {
        let loc = &symbol.location;
        let end_line = loc.end_line.unwrap_or(loc.line);

        // Check if position is within symbol's full range
        let in_range = if loc.line == end_line {
            // Single-line symbol
            loc.line == line && column >= loc.column && loc.end_column.is_none_or(|ec| column <= ec)
        } else {
            // Multi-line symbol
            (line > loc.line && line < end_line)
                || (line == loc.line && column >= loc.column)
                || (line == end_line && loc.end_column.is_none_or(|ec| column <= ec))
        };

        if in_range {
            // Check children first for more specific match
            if let Some(name) = find_symbol_at_position(&symbol.children, line, column) {
                return Some(name);
            }
            return Some(symbol.name.clone());
        }
    }
    None
}
