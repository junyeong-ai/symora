//! Symbol-level edit command implementation
//!
//! Provides symbol-aware text editing operations.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::models::lsp::FindSymbolsOptions;

#[derive(Args, Debug)]
pub struct EditArgs {
    #[command(subcommand)]
    pub command: EditCommand,
}

#[derive(Subcommand, Debug)]
pub enum EditCommand {
    /// Replace text at a range
    Replace {
        /// Start location (file:line:column)
        start: String,

        /// End location (file:line:column) - if not provided, replaces to end of line
        #[arg(short, long)]
        end: Option<String>,

        /// New text to insert
        #[arg(short, long)]
        text: String,

        /// Dry run (show diff without applying)
        #[arg(long)]
        dry_run: bool,
    },

    /// Insert text after a symbol or position
    InsertAfter {
        /// File path (use with --symbol)
        #[arg(required_unless_present = "location")]
        file: Option<String>,

        /// Location (file:line:column)
        #[arg(conflicts_with = "file")]
        location: Option<String>,

        /// Symbol path (e.g., "Class/method")
        #[arg(short = 's', long, requires = "file")]
        symbol: Option<String>,

        /// Text to insert
        #[arg(short, long)]
        text: String,

        /// Dry run (show diff without applying)
        #[arg(long)]
        dry_run: bool,
    },

    /// Insert text before a symbol or position
    InsertBefore {
        /// File path (use with --symbol)
        #[arg(required_unless_present = "location")]
        file: Option<String>,

        /// Location (file:line:column)
        #[arg(conflicts_with = "file")]
        location: Option<String>,

        /// Symbol path (e.g., "Class/method")
        #[arg(short = 's', long, requires = "file")]
        symbol: Option<String>,

        /// Text to insert
        #[arg(short, long)]
        text: String,

        /// Dry run (show diff without applying)
        #[arg(long)]
        dry_run: bool,
    },

    /// Replace a symbol's body (by location or symbol path)
    Symbol {
        /// File path (use with --symbol option)
        #[arg(required_unless_present = "location")]
        file: Option<String>,

        /// Location pointing to the symbol (file:line:column)
        #[arg(conflicts_with = "file")]
        location: Option<String>,

        /// Symbol path (e.g., "Class/method")
        #[arg(short = 's', long, requires = "file")]
        symbol: Option<String>,

        /// New text for the symbol
        #[arg(short, long)]
        text: String,

        /// Dry run (show diff without applying)
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn execute(args: EditArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    match args.command {
        EditCommand::Replace {
            start,
            end,
            text,
            dry_run,
        } => {
            let start_loc = ParsedLocation::parse(&start)?.to_absolute()?;
            let end_loc = if let Some(end_str) = end {
                ParsedLocation::parse(&end_str)?.to_absolute()?
            } else {
                start_loc.clone()
            };

            let result = apply_replace(
                &start_loc.file,
                start_loc.line,
                start_loc.column,
                end_loc.line,
                end_loc.column,
                &text,
                dry_run,
            )?;

            ctx.print_success_flat(result);
        }

        EditCommand::InsertAfter {
            file,
            location,
            symbol,
            text,
            dry_run,
        } => {
            let (file_path, line, col) = resolve_target(app, file, location, symbol).await?;
            let result = apply_insert(&file_path, line, col, &text, false, dry_run)?;
            ctx.print_success_flat(result);
        }

        EditCommand::InsertBefore {
            file,
            location,
            symbol,
            text,
            dry_run,
        } => {
            let (file_path, line, col) = resolve_target(app, file, location, symbol).await?;
            let result = apply_insert(&file_path, line, col, &text, true, dry_run)?;
            ctx.print_success_flat(result);
        }

        EditCommand::Symbol {
            file,
            location,
            symbol,
            text,
            dry_run,
        } => {
            let (file_path, target_symbol) = resolve_symbol(app, file, location, symbol).await?;

            let start_line = target_symbol.location.line;
            let start_col = target_symbol.location.column;
            let end_line = target_symbol.location.end_line.unwrap_or(start_line);
            let end_col = target_symbol.location.end_column.unwrap_or(0);

            let result = apply_replace(
                &file_path, start_line, start_col, end_line, end_col, &text, dry_run,
            )?;

            ctx.print_success_flat(serde_json::json!({
                "symbol": target_symbol.name,
                "name_path": target_symbol.name_path,
                "kind": target_symbol.kind.to_string(),
                "edit": result
            }));
        }
    }

    Ok(())
}

/// Resolve target position from file+symbol or location
async fn resolve_target(
    app: &App,
    file: Option<String>,
    location: Option<String>,
    symbol_path: Option<String>,
) -> Result<(std::path::PathBuf, u32, u32)> {
    use crate::models::symbol::Symbol;

    if let Some(loc_str) = location {
        let loc = ParsedLocation::parse(&loc_str)?.to_absolute()?;
        return Ok((loc.file, loc.line, loc.column));
    }

    let file =
        file.ok_or_else(|| anyhow::anyhow!("File path is required when location is not provided"))?;
    let symbol_pattern =
        symbol_path.ok_or_else(|| anyhow::anyhow!("--symbol is required when using file"))?;

    let path = std::path::Path::new(&file);
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        app.root().join(path)
    };

    let mut symbols = app
        .lsp
        .find_symbols(&abs_path, FindSymbolsOptions::new().with_depth(10))
        .await?;
    Symbol::compute_paths_for_all(&mut symbols);

    let target = Symbol::find_by_path(&symbols, &symbol_pattern)
        .ok_or_else(|| anyhow::anyhow!("Symbol not found: {}", symbol_pattern))?;

    let end_line = target.location.end_line.unwrap_or(target.location.line);
    let end_col = target.location.end_column.unwrap_or(1);

    Ok((abs_path, end_line, end_col))
}

/// Resolve symbol from file+symbol or location
async fn resolve_symbol(
    app: &App,
    file: Option<String>,
    location: Option<String>,
    symbol_path: Option<String>,
) -> Result<(std::path::PathBuf, crate::models::symbol::Symbol)> {
    use crate::models::symbol::Symbol;

    if let Some(loc_str) = location {
        let loc = ParsedLocation::parse(&loc_str)?.to_absolute()?;
        let symbols = app
            .lsp
            .find_symbols(&loc.file, FindSymbolsOptions::default())
            .await?;

        let target = symbols
            .iter()
            .find(|s| {
                s.location.line <= loc.line && s.location.end_line.is_none_or(|end| end >= loc.line)
            })
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No symbol found at {}:{}:{}",
                    loc.file.display(),
                    loc.line,
                    loc.column
                )
            })?;

        return Ok((loc.file, target));
    }

    let file =
        file.ok_or_else(|| anyhow::anyhow!("File path is required when location is not provided"))?;
    let symbol_pattern =
        symbol_path.ok_or_else(|| anyhow::anyhow!("--symbol is required when using file"))?;

    let path = std::path::Path::new(&file);
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        app.root().join(path)
    };

    let mut symbols = app
        .lsp
        .find_symbols(&abs_path, FindSymbolsOptions::new().with_depth(10))
        .await?;
    Symbol::compute_paths_for_all(&mut symbols);

    let target = Symbol::find_by_path(&symbols, &symbol_pattern)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Symbol not found: {}", symbol_pattern))?;

    Ok((abs_path, target))
}

/// Apply a replace edit to a file
fn apply_replace(
    file: &Path,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
    new_text: &str,
    dry_run: bool,
) -> Result<serde_json::Value> {
    let content = fs::read_to_string(file).context("Failed to read file")?;
    let lines: Vec<&str> = content.lines().collect();

    // Convert 1-indexed to 0-indexed
    let start_line_idx = (start_line.saturating_sub(1)) as usize;
    let end_line_idx = (end_line.saturating_sub(1)) as usize;
    let start_col_idx = (start_col.saturating_sub(1)) as usize;
    let end_col_idx = if end_col == 0 {
        // If end_col is 0, replace to end of line
        lines.get(end_line_idx).map(|l| l.len()).unwrap_or(0)
    } else {
        (end_col.saturating_sub(1)) as usize
    };

    // Validate ranges
    if start_line_idx >= lines.len() {
        anyhow::bail!("Start line {} is out of range", start_line);
    }
    if end_line_idx >= lines.len() {
        anyhow::bail!("End line {} is out of range", end_line);
    }

    // Build the new content
    let mut result = String::new();

    // Add lines before the edit
    for (i, line) in lines.iter().enumerate() {
        if i < start_line_idx {
            result.push_str(line);
            result.push('\n');
        } else if i == start_line_idx {
            // Add content before the edit on the start line
            let safe_start = start_col_idx.min(line.len());
            result.push_str(&line[..safe_start]);

            // Add the new text
            result.push_str(new_text);

            // If single line edit, add content after the edit
            if start_line_idx == end_line_idx {
                let safe_end = end_col_idx.min(line.len());
                result.push_str(&line[safe_end..]);
                result.push('\n');
            }
        } else if i > start_line_idx && i < end_line_idx {
            // Skip lines within the edit range
            continue;
        } else if i == end_line_idx && start_line_idx != end_line_idx {
            // Add content after the edit on the end line
            let safe_end = end_col_idx.min(line.len());
            result.push_str(&line[safe_end..]);
            result.push('\n');
        } else if i > end_line_idx {
            result.push_str(line);
            result.push('\n');
        }
    }

    // Remove trailing newline if original didn't have one
    if !content.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }

    // Calculate what was replaced
    let old_text = if start_line_idx == end_line_idx {
        let line = lines[start_line_idx];
        let safe_start = start_col_idx.min(line.len());
        let safe_end = end_col_idx.min(line.len());
        line[safe_start..safe_end].to_string()
    } else {
        let mut old = String::new();
        for (idx, line) in lines
            .iter()
            .enumerate()
            .take(end_line_idx + 1)
            .skip(start_line_idx)
        {
            if idx == start_line_idx {
                let safe_start = start_col_idx.min(line.len());
                old.push_str(&line[safe_start..]);
                old.push('\n');
            } else if idx == end_line_idx {
                let safe_end = end_col_idx.min(line.len());
                old.push_str(&line[..safe_end]);
            } else {
                old.push_str(line);
                old.push('\n');
            }
        }
        old
    };

    if dry_run {
        Ok(serde_json::json!({
            "dry_run": true,
            "file": file.display().to_string(),
            "old_text": old_text,
            "new_text": new_text,
            "range": {
                "start": {"line": start_line, "column": start_col},
                "end": {"line": end_line, "column": end_col}
            }
        }))
    } else {
        fs::write(file, &result).context("Failed to write file")?;

        Ok(serde_json::json!({
            "applied": true,
            "file": file.display().to_string(),
            "old_text": old_text,
            "new_text": new_text,
            "range": {
                "start": {"line": start_line, "column": start_col},
                "end": {"line": end_line, "column": end_col}
            }
        }))
    }
}

/// Apply an insert edit to a file
fn apply_insert(
    file: &Path,
    line: u32,
    column: u32,
    text: &str,
    before: bool,
    dry_run: bool,
) -> Result<serde_json::Value> {
    let content = fs::read_to_string(file).context("Failed to read file")?;
    let lines: Vec<&str> = content.lines().collect();

    // Convert 1-indexed to 0-indexed
    let line_idx = (line.saturating_sub(1)) as usize;
    let col_idx = (column.saturating_sub(1)) as usize;

    if line_idx >= lines.len() {
        anyhow::bail!("Line {} is out of range", line);
    }

    // Build the new content
    let mut result = String::new();

    for (i, line_content) in lines.iter().enumerate() {
        if i == line_idx {
            let safe_col = col_idx.min(line_content.len());
            // Both before and after insert at the same position
            // The difference is semantic (for user clarity)
            result.push_str(&line_content[..safe_col]);
            result.push_str(text);
            result.push_str(&line_content[safe_col..]);
            result.push('\n');
        } else {
            result.push_str(line_content);
            result.push('\n');
        }
    }

    // Remove trailing newline if original didn't have one
    if !content.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }

    let mode = if before {
        "insert_before"
    } else {
        "insert_after"
    };

    if dry_run {
        Ok(serde_json::json!({
            "dry_run": true,
            "mode": mode,
            "file": file.display().to_string(),
            "text": text,
            "position": {"line": line, "column": column}
        }))
    } else {
        fs::write(file, &result).context("Failed to write file")?;

        Ok(serde_json::json!({
            "applied": true,
            "mode": mode,
            "file": file.display().to_string(),
            "text": text,
            "position": {"line": line, "column": column}
        }))
    }
}
