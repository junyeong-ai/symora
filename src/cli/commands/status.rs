use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::ServerStatusOutput;
use crate::models::lsp::ServerStatus;
use crate::models::symbol::Language;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Show detailed status including LSP server install hints
    #[arg(long)]
    pub detailed: bool,
}

#[derive(Serialize)]
struct StatusOutput {
    initialized: bool,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<ProjectInfo>,
    lsp_servers: Vec<ServerStatusOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symora_dir: Option<String>,
}

#[derive(Serialize)]
struct ProjectInfo {
    name: Option<String>,
    languages: Vec<String>,
}

pub async fn execute(args: StatusArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let status = app.project.status().await?;

    let project = status.project.map(|p| ProjectInfo {
        name: Some(p.name),
        languages: p.languages.iter().map(|l| l.to_string()).collect(),
    });

    // Collect LSP server status for all supported languages
    let mut lsp_servers = Vec::new();
    for lang in Language::all() {
        let server_status = app.lsp.server_status(lang).await;

        let (status_str, install_hint) = match &server_status {
            ServerStatus::Running => ("running", None),
            ServerStatus::Stopped => ("available", None),
            ServerStatus::NotInstalled { hint } => ("not_installed", hint.clone()),
            ServerStatus::NotSupported => continue,
        };

        lsp_servers.push(ServerStatusOutput {
            language: lang.to_string(),
            status: status_str.to_string(),
            error: None,
            install_hint: if args.detailed { install_hint } else { None },
        });
    }

    let response = StatusOutput {
        initialized: status.initialized,
        // The resolved project root, absolute on purpose: project-relative
        // rendering would collapse it to "", which tells an agent nothing
        // about which project answered.
        path: app.root().display().to_string(),
        project,
        lsp_servers,
        symora_dir: if args.detailed {
            Some(ctx.relative_path(&app.root().join(".symora")))
        } else {
            None
        },
    };

    ctx.print_success(response);
    Ok(())
}
