//! Status command implementation
//!
//! Show project status and LSP server availability.

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
struct StatusResponse {
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

    // Collect LSP server status
    let languages = [
        Language::Rust,
        Language::TypeScript,
        Language::Python,
        Language::Go,
        Language::Java,
        Language::Kotlin,
    ];

    let mut lsp_servers = Vec::new();
    for lang in languages {
        let server_status = app.lsp.server_status(lang).await;

        let (status_str, name, install_hint) = match &server_status {
            ServerStatus::Running => ("running", None, None),
            ServerStatus::Starting => ("starting", None, None),
            ServerStatus::Stopped => ("available", None, None),
            ServerStatus::NotInstalled { hint } => ("not_installed", None, hint.clone()),
            ServerStatus::NotSupported => continue,
            ServerStatus::Error(e) => ("error", Some(e.clone()), None),
        };

        lsp_servers.push(ServerStatusOutput {
            language: lang.to_string(),
            status: status_str.to_string(),
            name,
            install_hint: if args.detailed { install_hint } else { None },
        });
    }

    let response = StatusResponse {
        initialized: status.initialized,
        path: ctx.relative_path(app.root()),
        project,
        lsp_servers,
        symora_dir: if args.detailed {
            Some(ctx.relative_path(&app.root().join(".symora")))
        } else {
            None
        },
    };

    ctx.print_success_flat(response);
    Ok(())
}
