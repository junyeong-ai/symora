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

/// A server's state as this overview can know it, which is not the same as
/// what it can do.
///
/// `status` reads the table and the session; it does not spawn a server to
/// find out whether it serves this workspace. So a resolved binary is
/// reported as installed — the fact this command established — and the
/// capability question belongs to `doctor`, which probes for it.
fn describe(status: &ServerStatus) -> (&'static str, Option<String>, Option<String>) {
    match status {
        ServerStatus::Running => ("running", None, None),
        ServerStatus::Stopped => ("installed", None, None),
        ServerStatus::NotInstalled { hint } => ("not_installed", hint.clone(), None),
        // The give-up reason is always surfaced (not detailed-gated): a
        // broken server is an error an agent must see to stop retrying.
        ServerStatus::CriticalFailure { reason } => {
            ("critical_failure", None, Some(reason.clone()))
        }
        ServerStatus::NotSupported => ("not_supported", None, None),
    }
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

        let (status_str, install_hint, error) = match &server_status {
            ServerStatus::NotSupported => continue,
            other => describe(other),
        };

        lsp_servers.push(ServerStatusOutput {
            language: lang.to_string(),
            status: status_str.to_string(),
            error,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A binary that resolves and a server that answers are different facts,
    /// and this command only ever established the first. Reporting the second
    /// would send an agent into a language it cannot navigate.
    #[test]
    fn a_resolved_binary_is_never_reported_as_a_capability() {
        assert_eq!(describe(&ServerStatus::Stopped).0, "installed");
        assert_eq!(describe(&ServerStatus::Running).0, "running");
        assert_eq!(
            describe(&ServerStatus::NotInstalled { hint: None }).0,
            "not_installed"
        );
        assert_eq!(
            describe(&ServerStatus::CriticalFailure {
                reason: "stayed unhealthy".to_string()
            })
            .0,
            "critical_failure"
        );
    }
}
