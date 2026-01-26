//! Code Actions command - List and apply LSP code actions (refactoring, quickfix, etc.)

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::commands::edit::apply_workspace_edits;
use crate::cli::response::Section;

#[derive(Args, Debug)]
pub struct ActionsArgs {
    #[command(subcommand)]
    pub command: ActionsCommand,
}

#[derive(Subcommand, Debug)]
pub enum ActionsCommand {
    /// List available code actions at location
    List(ListArgs),
    /// Apply a code action by title
    Apply(ApplyArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// Filter by action kind (quickfix, refactor, source)
    #[arg(long, short = 'k')]
    pub kind: Option<String>,

    /// Show only preferred actions
    #[arg(long)]
    pub preferred: bool,
}

#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// Action title to apply (use 'list' to see available actions)
    pub title: String,

    /// Preview changes without applying
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ActionOutput {
    title: String,
    kind: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_preferred: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ApplyResponse {
    title: String,
    kind: String,
    applied: bool,
    files_changed: usize,
    changes: Vec<FileChangeOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct FileChangeOutput {
    file: String,
    edit_count: usize,
}

pub async fn execute(args: ActionsArgs, app: &App) -> Result<()> {
    match args.command {
        ActionsCommand::List(list_args) => execute_list(list_args, app).await,
        ActionsCommand::Apply(apply_args) => execute_apply(apply_args, app).await,
    }
}

async fn execute_list(args: ListArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app.lsp.code_actions(&loc.file, loc.line, loc.column).await {
        Ok(actions) => {
            let filtered: Vec<_> = actions
                .into_iter()
                .filter(|a| {
                    args.kind
                        .as_ref()
                        .is_none_or(|kind_filter| a.kind.to_string().starts_with(kind_filter))
                })
                .filter(|a| !args.preferred || a.is_preferred)
                .collect();

            let output: Vec<ActionOutput> = filtered
                .iter()
                .map(|a| ActionOutput {
                    title: a.title.clone(),
                    kind: a.kind.to_string(),
                    is_preferred: a.is_preferred,
                    diagnostics: a.diagnostics.clone(),
                })
                .collect();

            ctx.print_success_flat(Section::new(output));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

async fn execute_apply(args: ApplyArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;

    match app.lsp.code_actions(&loc.file, loc.line, loc.column).await {
        Ok(actions) => {
            // Find matching action by title (case-insensitive partial match)
            let title_lower = args.title.to_lowercase();
            let action = match actions
                .iter()
                .find(|a| a.title.to_lowercase().contains(&title_lower))
            {
                Some(a) => a,
                None => {
                    let available: Vec<_> = actions.iter().map(|a| a.title.as_str()).collect();
                    ctx.print_error(&format!(
                        "No action matching '{}' found. Available: {:?}",
                        args.title, available
                    ));
                    return Ok(());
                }
            };

            match app.lsp.apply_code_action(&loc.file, action).await {
                Ok(result) => {
                    // Actually apply the edits to files
                    match apply_workspace_edits(&result.changes, args.dry_run) {
                        Ok(applied_changes) => {
                            let response = ApplyResponse {
                                title: action.title.clone(),
                                kind: action.kind.to_string(),
                                applied: !args.dry_run,
                                files_changed: applied_changes.len(),
                                changes: applied_changes
                                    .iter()
                                    .map(|c| FileChangeOutput {
                                        file: ctx.relative_path(&c.file),
                                        edit_count: c.edit_count,
                                    })
                                    .collect(),
                                message: if args.dry_run {
                                    Some("Dry run - no changes applied".to_string())
                                } else {
                                    None
                                },
                            };
                            ctx.print_success_flat(response);
                        }
                        Err(e) => ctx.print_error(&format!("Failed to apply action: {}", e)),
                    }
                }
                Err(e) => ctx.print_error(&e.to_string()),
            }
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
