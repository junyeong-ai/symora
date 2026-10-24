use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::ParsedLocation;
use crate::cli::commands::edit::apply_workspace_edits;
use crate::cli::commands::edit::refresh_store_files;
use crate::cli::response::{ActionOutput, ApplyActionOutput, FileChangeOutput, Section};

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
    /// File path with position (file:line[:column])
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
    /// File path with position (file:line[:column])
    pub location: String,

    /// Exact, unique action title to apply (use 'list' to see available actions)
    pub title: String,

    /// Preview changes without applying
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn execute(args: ActionsArgs, app: &App) -> Result<()> {
    match args.command {
        ActionsCommand::List(list_args) => execute_list(list_args, app).await,
        ActionsCommand::Apply(apply_args) => execute_apply(apply_args, app).await,
    }
}

async fn execute_list(args: ListArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute_with_root(Some(app.root()))?;

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

            ctx.print_success(Section::new(output));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

async fn execute_apply(args: ApplyArgs, app: &App) -> Result<()> {
    let _lock = (!args.dry_run)
        .then(|| super::edit::lock_edits(app.root()))
        .transpose()?;
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute_with_root(Some(app.root()))?;

    match app.lsp.code_actions(&loc.file, loc.line, loc.column).await {
        Ok(actions) => {
            let action = match select_action(&actions, &args.title) {
                Ok(action) => action,
                Err(error) => {
                    ctx.print_error(error);
                    return Ok(());
                }
            };

            match app.lsp.apply_code_action(&loc.file, action).await {
                Ok(result) => {
                    match apply_workspace_edits(&result.changes, args.dry_run, app.root()) {
                        Ok(applied_changes) => {
                            if !args.dry_run {
                                let changed_files: Vec<_> =
                                    applied_changes.iter().map(|c| c.file.clone()).collect();
                                refresh_store_files(app, &changed_files).await;
                            }

                            let response = ApplyActionOutput {
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
                            ctx.print_success(response);
                        }
                        Err(e) => ctx.print_error(e),
                    }
                }
                Err(e) => ctx.print_error(e),
            }
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

fn select_action<'a>(
    actions: &'a [crate::models::lsp::CodeAction],
    title: &str,
) -> Result<&'a crate::models::lsp::CodeAction, OutputError> {
    let mut matches = actions.iter().filter(|action| action.title == title);
    let action = matches.next().ok_or_else(|| {
        OutputError::not_found(format!("No action with exact title '{title}' found"))
            .with_hint("List the actions at this location and pass an exact title.")
    })?;
    if matches.next().is_some() {
        return Err(OutputError::invalid(format!(
            "Multiple actions have the title '{title}'; refusing to choose one"
        )));
    }
    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::lsp::{CodeAction, CodeActionKind};

    #[test]
    fn action_selection_requires_a_unique_exact_title() {
        let action = |title: &str| CodeAction {
            expected_hash: None,
            document_hashes: Default::default(),
            title: title.to_string(),
            kind: CodeActionKind::QuickFix,
            is_preferred: false,
            diagnostics: vec![],
            edit: None,
            data: None,
        };
        let actions = [
            action("Remove unused import"),
            action("Remove unused imports"),
        ];
        assert_eq!(
            select_action(&actions, "Remove unused import")
                .unwrap()
                .title,
            "Remove unused import"
        );
        assert!(select_action(&actions, "Remove unused").is_err());
        assert!(select_action(&[action("Fix"), action("Fix")], "Fix").is_err());
    }
}
