//! Actions command implementation
//!
//! List and apply LSP code actions (quickfix, refactor, source).
//! Code actions provide semantically correct transformations from the language server.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::LocationOutput;

#[derive(Args, Debug)]
pub struct ActionsArgs {
    #[command(subcommand)]
    pub command: ActionsCommand,
}

#[derive(Subcommand, Debug)]
pub enum ActionsCommand {
    /// List available code actions at position
    List {
        /// File path with position (file:line:column)
        location: String,

        /// Filter by action kind (quickfix, refactor, source)
        #[arg(long)]
        kind: Option<String>,
    },

    /// Apply a code action
    Apply {
        /// File path with position (file:line:column)
        location: String,

        /// Action index from list (0-based)
        #[arg(long)]
        index: Option<usize>,

        /// Apply the preferred action automatically
        #[arg(long)]
        preferred: bool,

        /// Filter by action kind before selecting
        #[arg(long)]
        kind: Option<String>,

        /// Actually execute the changes (default: dry-run showing diff)
        #[arg(long)]
        execute: bool,
    },
}

#[derive(Serialize)]
struct ActionsListResponse {
    location: LocationOutput,
    count: usize,
    actions: Vec<ActionOutput>,
}

#[derive(Serialize)]
struct ActionOutput {
    index: usize,
    title: String,
    kind: String,
    is_preferred: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<String>,
}

#[derive(Serialize)]
struct ApplyResponse {
    action: String,
    dry_run: bool,
    changes: Vec<FileChangeOutput>,
}

#[derive(Serialize)]
struct FileChangeOutput {
    file: String,
    edits: Vec<EditOutput>,
}

#[derive(Serialize)]
struct EditOutput {
    range: RangeOutput,
    new_text: String,
}

#[derive(Serialize)]
struct RangeOutput {
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

pub async fn execute(args: ActionsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    match args.command {
        ActionsCommand::List { location, kind } => {
            let loc = ParsedLocation::parse(&location)?.to_absolute()?;

            match app.lsp.code_actions(&loc.file, loc.line, loc.column).await {
                Ok(actions) => {
                    let filtered: Vec<_> = if let Some(ref k) = kind {
                        actions
                            .into_iter()
                            .filter(|a| {
                                a.kind
                                    .to_string()
                                    .to_lowercase()
                                    .contains(&k.to_lowercase())
                            })
                            .collect()
                    } else {
                        actions
                    };

                    let response = ActionsListResponse {
                        location: LocationOutput::from_path(
                            &loc.file,
                            loc.line,
                            loc.column,
                            ctx.root(),
                        ),
                        count: filtered.len(),
                        actions: filtered
                            .iter()
                            .enumerate()
                            .map(|(i, a)| ActionOutput {
                                index: i,
                                title: a.title.clone(),
                                kind: a.kind.to_string(),
                                is_preferred: a.is_preferred,
                                diagnostics: a.diagnostics.clone(),
                            })
                            .collect(),
                    };
                    ctx.print_success_flat(response);
                }
                Err(e) => ctx.print_error(&e.to_string()),
            }
        }

        ActionsCommand::Apply {
            location,
            index,
            preferred,
            kind,
            execute: do_execute,
        } => {
            let loc = ParsedLocation::parse(&location)?.to_absolute()?;

            // Get available actions
            let actions = match app.lsp.code_actions(&loc.file, loc.line, loc.column).await {
                Ok(a) => a,
                Err(e) => {
                    ctx.print_error(&e.to_string());
                    return Ok(());
                }
            };

            if actions.is_empty() {
                ctx.print_error("No code actions available at this position");
                return Ok(());
            }

            // Filter by kind if specified
            let filtered: Vec<_> = if let Some(ref k) = kind {
                actions
                    .into_iter()
                    .filter(|a| {
                        a.kind
                            .to_string()
                            .to_lowercase()
                            .contains(&k.to_lowercase())
                    })
                    .collect()
            } else {
                actions
            };

            if filtered.is_empty() {
                ctx.print_error("No code actions match the specified filter");
                return Ok(());
            }

            // Select action
            let selected = if preferred {
                // Find preferred action
                filtered
                    .iter()
                    .find(|a| a.is_preferred)
                    .or_else(|| filtered.first())
            } else if let Some(idx) = index {
                filtered.get(idx)
            } else {
                // Default to first action if no selection specified
                ctx.print_error(
                    "Specify --index or --preferred to select an action. Use 'actions list' to see available actions.",
                );
                return Ok(());
            };

            let action = match selected {
                Some(a) => a,
                None => {
                    ctx.print_error(&format!(
                        "Action index out of range. Available: 0-{}",
                        filtered.len() - 1
                    ));
                    return Ok(());
                }
            };

            // Apply action
            match app.lsp.apply_code_action(&loc.file, action).await {
                Ok(result) => {
                    let changes: Vec<FileChangeOutput> = result
                        .changes
                        .iter()
                        .map(|fc| FileChangeOutput {
                            file: ctx.relative_path(&fc.file),
                            edits: fc
                                .edits
                                .iter()
                                .map(|e| EditOutput {
                                    range: RangeOutput {
                                        start_line: e.range.start.line + 1,
                                        start_column: e.range.start.character + 1,
                                        end_line: e.range.end.line + 1,
                                        end_column: e.range.end.character + 1,
                                    },
                                    new_text: e.new_text.clone(),
                                })
                                .collect(),
                        })
                        .collect();

                    if do_execute {
                        // Apply changes to files
                        for change in &result.changes {
                            if let Err(e) = apply_edits_to_file(&change.file, &change.edits).await {
                                ctx.print_error(&format!(
                                    "Failed to apply changes to {}: {}",
                                    change.file.display(),
                                    e
                                ));
                                return Ok(());
                            }
                        }
                    }

                    let response = ApplyResponse {
                        action: action.title.clone(),
                        dry_run: !do_execute,
                        changes,
                    };
                    ctx.print_success_flat(response);
                }
                Err(e) => ctx.print_error(&e.to_string()),
            }
        }
    }

    Ok(())
}

/// Apply text edits to a file
async fn apply_edits_to_file(
    file: &std::path::Path,
    edits: &[crate::models::lsp::TextEdit],
) -> Result<()> {
    use std::io::Write;

    let content = tokio::fs::read_to_string(file).await?;
    let lines: Vec<&str> = content.lines().collect();

    // Sort edits in reverse order (bottom to top) to preserve positions
    let mut sorted_edits: Vec<_> = edits.iter().collect();
    sorted_edits.sort_by(|a, b| {
        let a_pos = (a.range.start.line, a.range.start.character);
        let b_pos = (b.range.start.line, b.range.start.character);
        b_pos.cmp(&a_pos) // Reverse order
    });

    // Convert to byte offsets and apply
    let mut result = content.clone();

    for edit in sorted_edits {
        let start_offset =
            line_col_to_offset(&lines, edit.range.start.line, edit.range.start.character);
        let end_offset = line_col_to_offset(&lines, edit.range.end.line, edit.range.end.character);

        if let (Some(start), Some(end)) = (start_offset, end_offset) {
            result = format!("{}{}{}", &result[..start], edit.new_text, &result[end..]);
        }
    }

    // Write back to file
    let mut file = std::fs::File::create(file)?;
    file.write_all(result.as_bytes())?;

    Ok(())
}

/// Convert line:column to byte offset
fn line_col_to_offset(lines: &[&str], line: u32, col: u32) -> Option<usize> {
    let mut offset = 0;

    for (i, l) in lines.iter().enumerate() {
        if i == line as usize {
            return Some(offset + col as usize);
        }
        offset += l.len() + 1; // +1 for newline
    }

    // Handle end of file
    if line as usize == lines.len() {
        Some(offset)
    } else {
        None
    }
}
