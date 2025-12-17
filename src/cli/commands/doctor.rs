//! Doctor command - dependency and LSP server health check

use std::process::Command;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::infra::lsp::capabilities::{LspFeature, SupportLevel, get_support_level};
use crate::infra::lsp::servers::{ServerHealth, check_all_servers};

#[derive(Args, Debug)]
pub struct DoctorArgs {
    #[arg(long)]
    pub missing_only: bool,
}

#[derive(Serialize)]
struct DoctorResponse {
    summary: DoctorSummary,
    tools: Vec<ToolEntry>,
    servers: Vec<ServerEntry>,
}

#[derive(Serialize)]
struct DoctorSummary {
    tools_installed: usize,
    tools_missing: usize,
    servers_installed: usize,
    servers_missing: usize,
}

#[derive(Serialize)]
struct ToolEntry {
    name: String,
    description: String,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_command: Option<String>,
}

#[derive(Serialize)]
struct ServerEntry {
    language: String,
    name: String,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    limited_features: Vec<String>,
}

fn get_limited_features(health: &ServerHealth) -> Vec<String> {
    let features = [
        LspFeature::FindReferences,
        LspFeature::GotoDefinition,
        LspFeature::GotoTypeDefinition,
        LspFeature::FindImplementations,
        LspFeature::IncomingCalls,
        LspFeature::OutgoingCalls,
        LspFeature::Rename,
        LspFeature::CodeActions,
    ];

    features
        .iter()
        .filter_map(|&feature| {
            let level = get_support_level(health.language, feature);
            match level {
                SupportLevel::None => Some(format!("{}: not supported", feature.display_name())),
                SupportLevel::Partial => Some(format!("{}: limited", feature.display_name())),
                SupportLevel::Full => None,
            }
        })
        .collect()
}

impl From<ServerHealth> for ServerEntry {
    fn from(health: ServerHealth) -> Self {
        let limited = get_limited_features(&health);
        Self {
            language: format!("{:?}", health.language),
            name: health.name.to_string(),
            installed: health.installed,
            version: health.version,
            install_command: if health.installed {
                None
            } else {
                Some(health.install_instruction)
            },
            limited_features: limited,
        }
    }
}

fn check_ripgrep() -> ToolEntry {
    let output = Command::new("rg").arg("--version").output();

    match output {
        Ok(output) if output.status.success() => {
            let version_str = String::from_utf8_lossy(&output.stdout);
            let version = version_str
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .map(String::from);

            ToolEntry {
                name: "ripgrep".to_string(),
                description: "Fast regex search (required for 'search text')".to_string(),
                installed: true,
                version,
                install_command: None,
            }
        }
        _ => ToolEntry {
            name: "ripgrep".to_string(),
            description: "Fast regex search (required for 'search text')".to_string(),
            installed: false,
            version: None,
            install_command: Some(get_ripgrep_install_command()),
        },
    }
}

fn get_ripgrep_install_command() -> String {
    #[cfg(target_os = "macos")]
    {
        "brew install ripgrep".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        "apt install ripgrep  OR  cargo install ripgrep".to_string()
    }
    #[cfg(target_os = "windows")]
    {
        "choco install ripgrep  OR  cargo install ripgrep".to_string()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "cargo install ripgrep".to_string()
    }
}

pub fn execute(args: DoctorArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    let ripgrep = check_ripgrep();
    let tools: Vec<ToolEntry> = vec![ripgrep]
        .into_iter()
        .filter(|t| !args.missing_only || !t.installed)
        .collect();

    let health_results = check_all_servers();
    let servers: Vec<ServerEntry> = health_results
        .into_iter()
        .filter(|h| !args.missing_only || !h.installed)
        .map(ServerEntry::from)
        .collect();

    let tools_installed = tools.iter().filter(|t| t.installed).count();
    let tools_missing = tools.iter().filter(|t| !t.installed).count();
    let servers_installed = servers.iter().filter(|s| s.installed).count();
    let servers_missing = servers.iter().filter(|s| !s.installed).count();

    let response = DoctorResponse {
        summary: DoctorSummary {
            tools_installed,
            tools_missing,
            servers_installed,
            servers_missing,
        },
        tools,
        servers,
    };

    ctx.print_success_flat(response);
    Ok(())
}
