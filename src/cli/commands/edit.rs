use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::ParsedLocation;
#[cfg(unix)]
use crate::daemon::DaemonClient;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol};

/// Maximum file size for editing (100MB)
const MAX_EDIT_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// Validate file is suitable for editing (exists, readable, writable, size limit)
fn validate_file_for_edit(path: &Path) -> Result<()> {
    use crate::cli::CliInputError;

    if !path.exists() {
        anyhow::bail!(CliInputError::FileNotFound(path.to_path_buf()));
    }

    let metadata = fs::metadata(path).context("Failed to read file metadata")?;

    if metadata.len() > MAX_EDIT_FILE_SIZE {
        anyhow::bail!(CliInputError::FileTooLarge {
            size_mb: metadata.len() / (1024 * 1024),
            limit_mb: MAX_EDIT_FILE_SIZE / (1024 * 1024),
        });
    }

    if fs::OpenOptions::new().write(true).open(path).is_err() {
        anyhow::bail!(CliInputError::FileNotWritable(path.to_path_buf()));
    }

    Ok(())
}

use crate::utils::char_to_byte_index;

// Workspace Edit Applier

use crate::models::lsp::{FileChangeWithEdits, TextEdit};

/// Apply workspace edits from LSP to actual files.
/// Used by rename and actions apply commands.
///
/// Returns the number of files modified and details of changes.
pub fn apply_workspace_edits(
    changes: &[FileChangeWithEdits],
    dry_run: bool,
) -> Result<Vec<AppliedFileChange>> {
    let mut results = Vec::new();

    for change in changes {
        let file = &change.file;

        if !dry_run {
            validate_file_for_edit(file)?;
        }

        let content = fs::read_to_string(file)
            .with_context(|| format!("Failed to read file: {}", file.display()))?;

        let new_content = apply_text_edits(&content, &change.edits);

        if !dry_run {
            fs::write(file, &new_content)
                .with_context(|| format!("Failed to write file: {}", file.display()))?;
        }

        results.push(AppliedFileChange {
            file: file.clone(),
            edit_count: change.edits.len(),
            applied: !dry_run,
        });
    }

    Ok(results)
}

/// Result of applying edits to a single file
#[derive(Debug, Clone)]
pub struct AppliedFileChange {
    pub file: PathBuf,
    pub edit_count: usize,
    pub applied: bool,
}

/// Apply multiple text edits to content.
/// Edits are sorted by position (reverse order) to maintain correct offsets.
fn apply_text_edits(content: &str, edits: &[TextEdit]) -> String {
    if edits.is_empty() {
        return content.to_string();
    }

    // Sort edits by position in reverse order (bottom to top, right to left)
    // This ensures earlier edits don't affect the positions of later edits
    let mut sorted_edits: Vec<_> = edits.iter().collect();
    sorted_edits.sort_by(|a, b| {
        let a_start = (a.range.start.line, a.range.start.character);
        let b_start = (b.range.start.line, b.range.start.character);
        b_start.cmp(&a_start) // Reverse order
    });

    let mut result = content.to_string();

    for edit in sorted_edits {
        let start_line = edit.range.start.line as usize;
        let end_line = edit.range.end.line as usize;
        let start_char = edit.range.start.character as usize;
        let end_char = edit.range.end.character as usize;

        // Calculate byte offsets
        let start_offset = line_char_to_byte_offset(&result, start_line, start_char);
        let end_offset = line_char_to_byte_offset(&result, end_line, end_char);

        if start_offset <= end_offset && end_offset <= result.len() {
            result.replace_range(start_offset..end_offset, &edit.new_text);
        } else {
            // If offsets are invalid, skip this edit with a warning
            tracing::warn!(
                "Invalid edit range: {:?} -> {:?} in content of {} bytes",
                edit.range,
                (start_offset, end_offset),
                result.len()
            );
        }
    }

    result
}

fn line_char_to_byte_offset(content: &str, line: usize, character: usize) -> usize {
    let mut byte_offset = 0;

    for (current_line, line_content) in content.lines().enumerate() {
        if current_line == line {
            return byte_offset + char_to_byte_index(line_content, character);
        }
        byte_offset += line_content.len();
        // Account for actual line ending bytes (\r\n = 2, \n = 1)
        let remaining = &content.as_bytes()[byte_offset..];
        if remaining.starts_with(b"\r\n") {
            byte_offset += 2;
        } else if remaining.starts_with(b"\n") {
            byte_offset += 1;
        }
    }

    content.len()
}

/// Detect the line ending style used in content
fn detect_line_ending(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

// Target Resolution Helpers

/// Specifies position relative to symbol for insert operations
#[derive(Clone, Copy)]
enum InsertMode {
    /// Insert after symbol (at end position)
    After,
    /// Insert before symbol (at start position)
    Before,
}

/// Resolve file path from target string, handling relative paths.
/// Canonicalizes the result and validates it is within the project root.
fn resolve_file_path(app: &App, target: &str) -> Result<PathBuf> {
    let path = Path::new(target);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        app.root().join(path)
    };
    let canonical = resolved
        .canonicalize()
        .with_context(|| format!("Cannot resolve path: {}", resolved.display()))?;
    let root = app
        .root()
        .canonicalize()
        .unwrap_or_else(|_| app.root().to_path_buf());
    if !canonical.starts_with(&root) {
        anyhow::bail!(crate::cli::CliInputError::PathOutsideProject(canonical));
    }
    Ok(canonical)
}

/// Find symbol by path pattern in a file
async fn find_symbol_by_path(app: &App, file: &Path, pattern: &str) -> Result<Symbol> {
    let mut symbols = app
        .lsp
        .find_symbols(file, FindSymbolsOptions::default().with_depth(10))
        .await?;
    Symbol::compute_paths_for_all(&mut symbols);
    Symbol::find_by_path(&symbols, pattern)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Symbol not found: {}", pattern))
}

/// Find the symbol containing a specific line position
async fn find_symbol_at_line(app: &App, file: &Path, line: u32) -> Result<Symbol> {
    let symbols = app
        .lsp
        .find_symbols(file, FindSymbolsOptions::default())
        .await?;
    crate::cli::utils::find_symbol_at_position(&symbols, line, None)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("No symbol found at line {}", line))
}

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
        /// Target: `file:line[:col]` (location) or file path (with --symbol)
        target: String,

        /// Symbol path when target is a file (e.g., "Class/method")
        #[arg(short = 's', long)]
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
        /// Target: `file:line[:col]` (location) or file path (with --symbol)
        target: String,

        /// Symbol path when target is a file (e.g., "Class/method")
        #[arg(short = 's', long)]
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
        /// Target: `file:line[:col]` (location) or file path (with --symbol)
        target: String,

        /// Symbol path when target is a file (e.g., "Class/method")
        #[arg(short = 's', long)]
        symbol: Option<String>,

        /// New text for the symbol
        #[arg(short, long)]
        text: String,

        /// Dry run (show diff without applying)
        #[arg(long)]
        dry_run: bool,
    },

    /// Replace code matching an AST pattern (tree-sitter)
    Pattern {
        /// File path to edit
        file: String,

        /// Tree-sitter pattern (e.g., "(function_item name: (identifier) @name)")
        #[arg(short, long)]
        pattern: String,

        /// Language for tree-sitter grammar
        #[arg(short, long)]
        lang: String,

        /// New text to replace matched code
        #[arg(short, long)]
        text: String,

        /// Match index to replace (0-indexed); use "all" to replace all matches
        #[arg(short, long, default_value = "0")]
        index: String,

        /// Dry run (show matches without applying)
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn execute(args: EditArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    // Track edited file for store invalidation (unix only)
    #[cfg(unix)]
    let mut edited_file: Option<PathBuf> = None;

    match args.command {
        EditCommand::Replace {
            start,
            end,
            text,
            dry_run,
        } => {
            let start_loc =
                ParsedLocation::parse(&start)?.to_absolute_with_root(Some(app.root()))?;
            let end_loc = if let Some(end_str) = end {
                ParsedLocation::parse(&end_str)?.to_absolute_with_root(Some(app.root()))?
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

            #[cfg(unix)]
            if !dry_run {
                edited_file = Some(start_loc.file.clone());
            }
            ctx.print_success(result);
        }

        EditCommand::InsertAfter {
            target,
            symbol,
            text,
            dry_run,
        } => {
            let (file_path, line, col) =
                resolve_insert_position(app, &target, symbol, InsertMode::After).await?;
            let result = apply_insert(&file_path, line, col, &text, false, dry_run)?;
            #[cfg(unix)]
            if !dry_run {
                edited_file = Some(file_path);
            }
            ctx.print_success(result);
        }

        EditCommand::InsertBefore {
            target,
            symbol,
            text,
            dry_run,
        } => {
            let (file_path, line, col) =
                resolve_insert_position(app, &target, symbol, InsertMode::Before).await?;
            let result = apply_insert(&file_path, line, col, &text, true, dry_run)?;
            #[cfg(unix)]
            if !dry_run {
                edited_file = Some(file_path);
            }
            ctx.print_success(result);
        }

        EditCommand::Symbol {
            target,
            symbol,
            text,
            dry_run,
        } => {
            let (file_path, target_symbol) = resolve_symbol(app, &target, symbol).await?;

            // Use effective_start() to get the full declaration start position
            // This includes modifiers like "pub fn", not just the symbol name
            let (start_line, start_col) = target_symbol.location.effective_start();
            let end_line = target_symbol.location.end_line.unwrap_or(start_line);
            let end_col = target_symbol.location.end_column.unwrap_or(0);

            let result = apply_replace(
                &file_path, start_line, start_col, end_line, end_col, &text, dry_run,
            )?;

            #[cfg(unix)]
            if !dry_run {
                edited_file = Some(file_path);
            }
            ctx.print_success(serde_json::json!({
                "symbol": target_symbol.name,
                "name_path": target_symbol.name_path,
                "kind": target_symbol.kind.to_string(),
                "edit": result
            }));
        }

        EditCommand::Pattern {
            file,
            pattern,
            lang,
            text,
            index,
            dry_run,
        } => {
            let abs_path = resolve_file_path(app, &file)?;
            let result =
                execute_pattern_edit(app, &abs_path, &pattern, &lang, &text, &index, dry_run)
                    .await?;
            #[cfg(unix)]
            if !dry_run {
                edited_file = Some(abs_path);
            }
            ctx.print_success(result);
        }
    }

    // Best-effort store invalidation after successful edits
    #[cfg(unix)]
    if let Some(file) = edited_file {
        invalidate_store_files(app, &[file]).await;
    }

    Ok(())
}

/// Best-effort invalidation of files in the daemon's store index.
#[cfg(unix)]
pub(crate) async fn invalidate_store_files(app: &App, files: &[PathBuf]) {
    let client = DaemonClient::new(app.root());
    for file in files {
        if let Err(e) = client.invalidate_file(file).await {
            tracing::warn!("Store invalidation failed for {}: {}", file.display(), e);
        }
    }
}

/// Resolve position for insert operations with mode-aware positioning.
/// Auto-detects location format (`file:line[:col]`) vs file path (requires --symbol).
async fn resolve_insert_position(
    app: &App,
    target: &str,
    symbol_path: Option<String>,
    mode: InsertMode,
) -> Result<(PathBuf, u32, u32)> {
    // Location mode: use specified position directly
    if ParsedLocation::is_location_format(target) {
        let loc = ParsedLocation::parse(target)?.to_absolute_with_root(Some(app.root()))?;
        return Ok((loc.file, loc.line, loc.column));
    }

    // Symbol mode: --symbol is required
    let pattern = symbol_path.ok_or_else(|| {
        anyhow::anyhow!(
            "--symbol is required when target is a file path. \
             Use file:line:col format for position-based editing."
        )
    })?;

    let file = resolve_file_path(app, target)?;
    let symbol = find_symbol_by_path(app, &file, &pattern).await?;

    // Mode-aware position selection (fixes InsertBefore bug)
    let (line, column) = match mode {
        InsertMode::After => (
            symbol.location.end_line.unwrap_or(symbol.location.line),
            symbol.location.end_column.unwrap_or(1),
        ),
        InsertMode::Before => (symbol.location.line, symbol.location.column),
    };

    Ok((file, line, column))
}

/// Resolve full symbol from target for operations needing complete symbol info.
/// Auto-detects location format (`file:line[:col]`) vs file path (requires --symbol).
async fn resolve_symbol(
    app: &App,
    target: &str,
    symbol_path: Option<String>,
) -> Result<(PathBuf, Symbol)> {
    // Location mode: find symbol at position
    if ParsedLocation::is_location_format(target) {
        let loc = ParsedLocation::parse(target)?.to_absolute_with_root(Some(app.root()))?;
        let symbol = find_symbol_at_line(app, &loc.file, loc.line).await?;
        return Ok((loc.file, symbol));
    }

    // Symbol mode: --symbol is required
    let pattern = symbol_path.ok_or_else(|| {
        anyhow::anyhow!(
            "--symbol is required when target is a file path. \
             Use file:line:col format for position-based editing."
        )
    })?;

    let file = resolve_file_path(app, target)?;
    let symbol = find_symbol_by_path(app, &file, &pattern).await?;
    Ok((file, symbol))
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
    // Validate file before editing (includes size and write permission check)
    if !dry_run {
        validate_file_for_edit(file)?;
    }

    let content = fs::read_to_string(file).context("Failed to read file")?;
    let lines: Vec<&str> = content.lines().collect();

    // Convert 1-indexed to 0-indexed (character-based, not byte-based)
    let start_line_idx = (start_line.saturating_sub(1)) as usize;
    let end_line_idx = (end_line.saturating_sub(1)) as usize;
    let start_char_idx = (start_col.saturating_sub(1)) as usize;
    let end_char_idx = if end_col == 0 {
        // If end_col is 0, replace to end of line (use char count)
        lines
            .get(end_line_idx)
            .map(|l| l.chars().count())
            .unwrap_or(0)
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

    // Build the new content, preserving original line ending style
    let eol = detect_line_ending(&content);
    let mut result = String::new();

    // Add lines before the edit
    for (i, line) in lines.iter().enumerate() {
        if i < start_line_idx {
            result.push_str(line);
            result.push_str(eol);
        } else if i == start_line_idx {
            // Add content before the edit on the start line (UTF-8 safe)
            let safe_start_byte = char_to_byte_index(line, start_char_idx);
            result.push_str(&line[..safe_start_byte]);

            // Add the new text
            result.push_str(new_text);

            // If single line edit, add content after the edit
            if start_line_idx == end_line_idx {
                let safe_end_byte = char_to_byte_index(line, end_char_idx);
                result.push_str(&line[safe_end_byte..]);
                result.push_str(eol);
            }
        } else if i > start_line_idx && i < end_line_idx {
            // Skip lines within the edit range
            continue;
        } else if i == end_line_idx && start_line_idx != end_line_idx {
            // Add content after the edit on the end line (UTF-8 safe)
            let safe_end_byte = char_to_byte_index(line, end_char_idx);
            result.push_str(&line[safe_end_byte..]);
            result.push_str(eol);
        } else if i > end_line_idx {
            result.push_str(line);
            result.push_str(eol);
        }
    }

    // Remove trailing line ending if original didn't have one
    if !content.ends_with('\n') && result.ends_with('\n') {
        if result.ends_with("\r\n") {
            result.truncate(result.len() - 2);
        } else {
            result.pop();
        }
    }

    // Calculate what was replaced (UTF-8 safe)
    let old_text = if start_line_idx == end_line_idx {
        let line = lines[start_line_idx];
        let safe_start_byte = char_to_byte_index(line, start_char_idx);
        let safe_end_byte = char_to_byte_index(line, end_char_idx);
        line[safe_start_byte..safe_end_byte].to_string()
    } else {
        let mut old = String::new();
        for (idx, line) in lines
            .iter()
            .enumerate()
            .take(end_line_idx + 1)
            .skip(start_line_idx)
        {
            if idx == start_line_idx {
                let safe_start_byte = char_to_byte_index(line, start_char_idx);
                old.push_str(&line[safe_start_byte..]);
                old.push('\n');
            } else if idx == end_line_idx {
                let safe_end_byte = char_to_byte_index(line, end_char_idx);
                old.push_str(&line[..safe_end_byte]);
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
///
/// Behavior depends on whether a specific column is provided:
/// - Line-based (column=1): Insert as a new line before/after the target line
/// - Position-based (column>1): Insert at the specific position within the line
fn apply_insert(
    file: &Path,
    line: u32,
    column: u32,
    text: &str,
    before: bool,
    dry_run: bool,
) -> Result<serde_json::Value> {
    if !dry_run {
        validate_file_for_edit(file)?;
    }

    let content = fs::read_to_string(file).context("Failed to read file")?;
    let lines: Vec<&str> = content.lines().collect();
    let line_idx = (line.saturating_sub(1)) as usize;

    if line_idx >= lines.len() {
        anyhow::bail!("Line {} is out of range", line);
    }

    // Determine if this is a line-based or position-based insert
    // Line-based: column=1 means "insert as a new line"
    // Position-based: column>1 means "insert at this character position"
    let is_line_based = column == 1;

    let eol = detect_line_ending(&content);
    let mut result = String::new();

    if is_line_based {
        // Line-based insertion: add text as a new line
        for (i, line_content) in lines.iter().enumerate() {
            if before && i == line_idx {
                result.push_str(text);
                result.push_str(eol);
            }

            result.push_str(line_content);
            result.push_str(eol);

            if !before && i == line_idx {
                result.push_str(text);
                result.push_str(eol);
            }
        }
    } else {
        // Position-based insertion: insert at specific character position
        let char_idx = (column.saturating_sub(1)) as usize;

        for (i, line_content) in lines.iter().enumerate() {
            if i == line_idx {
                let safe_col_byte = char_to_byte_index(line_content, char_idx);
                result.push_str(&line_content[..safe_col_byte]);
                result.push_str(text);
                result.push_str(&line_content[safe_col_byte..]);
                result.push_str(eol);
            } else {
                result.push_str(line_content);
                result.push_str(eol);
            }
        }
    }

    // Preserve original trailing line ending behavior
    if !content.ends_with('\n') && result.ends_with('\n') {
        if result.ends_with("\r\n") {
            result.truncate(result.len() - 2);
        } else {
            result.pop();
        }
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
            "position": {"line": line, "column": column},
            "line_based": is_line_based
        }))
    } else {
        fs::write(file, &result).context("Failed to write file")?;

        Ok(serde_json::json!({
            "applied": true,
            "mode": mode,
            "file": file.display().to_string(),
            "text": text,
            "position": {"line": line, "column": column},
            "line_based": is_line_based
        }))
    }
}

/// Execute a pattern-based edit using tree-sitter AST matching
async fn execute_pattern_edit(
    app: &App,
    file: &Path,
    pattern: &str,
    lang: &str,
    new_text: &str,
    index: &str,
    dry_run: bool,
) -> Result<serde_json::Value> {
    let language = Language::parse_or_default(lang);
    if language == Language::Unknown {
        anyhow::bail!(
            "Unsupported language: {}. Run 'symora search nodes --list' for supported languages.",
            lang
        );
    }

    // Validate file before editing (skips write check for dry_run)
    if !dry_run {
        validate_file_for_edit(file)?;
    } else if !file.exists() {
        anyhow::bail!("File not found: {}", file.display());
    }

    let file_buf = file.to_path_buf();
    let matches = app
        .ast
        .query(pattern, language, std::slice::from_ref(&file_buf))
        .await
        .map_err(|e| anyhow::anyhow!("AST query failed: {}", e))?;

    if matches.is_empty() {
        return Ok(serde_json::json!({
            "matched": false,
            "pattern": pattern,
            "file": file.display().to_string(),
            "message": "No matches found for the pattern"
        }));
    }

    let replace_all = index.eq_ignore_ascii_case("all");
    let target_index: Option<usize> = if replace_all {
        None
    } else {
        Some(
            index
                .parse()
                .context("Invalid index: must be a number or 'all'")?,
        )
    };

    if let Some(idx) = target_index
        && idx >= matches.len()
    {
        anyhow::bail!(
            "Index {} out of range. Found {} matches.",
            idx,
            matches.len()
        );
    }

    let content = fs::read_to_string(file).context("Failed to read file")?;

    if dry_run {
        let match_info: Vec<_> = matches
            .iter()
            .enumerate()
            .map(|(i, m)| {
                serde_json::json!({
                    "index": i,
                    "line": m.start_line,
                    "end_line": m.end_line,
                    "column": m.start_column,
                    "end_column": m.end_column,
                    "text": m.text,
                    "will_replace": replace_all || target_index == Some(i)
                })
            })
            .collect();

        return Ok(serde_json::json!({
            "dry_run": true,
            "file": file.display().to_string(),
            "pattern": pattern,
            "language": lang,
            "matches": match_info,
            "replacement": new_text
        }));
    }

    // Apply edits in reverse order to preserve positions
    let mut edits_to_apply: Vec<_> = matches
        .iter()
        .enumerate()
        .filter(|(i, _)| replace_all || target_index == Some(*i))
        .collect();
    edits_to_apply.sort_by(|a, b| {
        (b.1.start_line, b.1.start_column).cmp(&(a.1.start_line, a.1.start_column))
    });

    let mut result_lines: Vec<String> = content.lines().map(String::from).collect();

    let mut applied_edits = Vec::new();

    for (idx, ast_match) in &edits_to_apply {
        let start_line_idx = (ast_match.start_line.saturating_sub(1)) as usize;
        let end_line_idx = (ast_match.end_line.saturating_sub(1)) as usize;

        if start_line_idx >= result_lines.len() || end_line_idx >= result_lines.len() {
            continue;
        }

        let start_col = ast_match.start_column as usize;
        let end_col = ast_match.end_column as usize;

        if start_line_idx == end_line_idx {
            let line = &result_lines[start_line_idx];
            let safe_start = start_col.min(line.len());
            let safe_end = end_col.min(line.len());
            let new_line = format!("{}{}{}", &line[..safe_start], new_text, &line[safe_end..]);
            result_lines[start_line_idx] = new_line;
        } else {
            let first_line = &result_lines[start_line_idx];
            let last_line = &result_lines[end_line_idx];
            let safe_start = start_col.min(first_line.len());
            let safe_end = end_col.min(last_line.len());

            let new_line = format!(
                "{}{}{}",
                &first_line[..safe_start],
                new_text,
                &last_line[safe_end..]
            );
            result_lines[start_line_idx] = new_line;

            // Use drain for O(n) removal instead of O(n²) individual removes
            result_lines.drain((start_line_idx + 1)..=end_line_idx);
        }

        applied_edits.push(serde_json::json!({
            "index": idx,
            "original": ast_match.text,
            "line": ast_match.start_line,
            "end_line": ast_match.end_line
        }));
    }

    let mut result = result_lines.join("\n");
    if content.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }

    fs::write(file, &result).context("Failed to write file")?;

    Ok(serde_json::json!({
        "applied": true,
        "file": file.display().to_string(),
        "pattern": pattern,
        "language": lang,
        "edits_applied": applied_edits.len(),
        "edits": applied_edits,
        "replacement": new_text
    }))
}
