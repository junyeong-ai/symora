use std::collections::HashMap;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::infra::lsp::servers::{self, Platform, ServerSource, check_all_servers};

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Check only specific language
    #[arg(value_name = "LANGUAGE")]
    pub language: Option<String>,
}

#[derive(Serialize)]
struct DoctorOutput {
    platform: String,
    languages: Vec<LanguageStatus>,
    summary: Summary,
    /// Config problems affecting this report: rejected [lsp.servers] keys
    /// (recorded at load, never applied) and/or a whole-config load
    /// failure (the report then reflects builtin defaults).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    config_errors: Vec<String>,
}

#[derive(Serialize)]
struct LanguageStatus {
    language: String,
    server: String,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    tier: String,
    /// Some("config") iff an [lsp.servers] override applies — what the
    /// next server start will use. Omitted for builtin servers; that
    /// absence is how an agent detects an override that did not apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    /// Effective spawn command; present iff `source` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    install: Option<String>,
}

#[derive(Serialize)]
struct Summary {
    total: usize,
    installed: usize,
    missing: usize,
}

pub async fn execute(args: DoctorArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    // Re-load through the same ConfigService every command uses, so the
    // report reflects on-disk config and can disclose load failures that
    // App::new falls back from silently.
    let (overrides, config_errors) = match app.config_service.load(false).await {
        Ok(config) => {
            let errors = config
                .lsp
                .server_override_errors
                .iter()
                .map(ToString::to_string)
                .collect();
            (config.lsp.servers, errors)
        }
        Err(e) => (HashMap::new(), vec![e.to_string()]),
    };
    let all_servers = check_all_servers(servers::merged(&overrides));

    let languages: Vec<LanguageStatus> = all_servers
        .into_iter()
        .filter(|s| {
            args.language
                .as_ref()
                .is_none_or(|filter| s.language.to_string().contains(&filter.to_lowercase()))
        })
        .map(|s| {
            let overridden = s.source == ServerSource::Config;
            let install = if s.installed {
                None
            } else if overridden {
                Some(format!(
                    "fix [lsp.servers.{}]: command `{}` not found or not executable — \
                     correct the path or remove the override to fall back to the builtin \
                     server",
                    s.language, s.command
                ))
            } else {
                Some(s.install_instruction.to_string())
            };
            LanguageStatus {
                language: s.language.to_string(),
                server: s.name.to_string(),
                installed: s.installed,
                version: s.version,
                tier: s.tier.as_str().to_string(),
                source: overridden.then(|| "config".to_string()),
                command: overridden.then_some(s.command),
                install,
            }
        })
        .collect();

    let installed_count = languages.iter().filter(|l| l.installed).count();
    let total = languages.len();

    let response = DoctorOutput {
        platform: platform_to_string(Platform::current()),
        summary: Summary {
            total,
            installed: installed_count,
            missing: total - installed_count,
        },
        languages,
        config_errors,
    };

    ctx.print_success(response);
    Ok(())
}

fn platform_to_string(platform: Platform) -> String {
    match platform {
        Platform::MacOS => "macos",
        Platform::Linux => "linux",
        Platform::Windows => "windows",
    }
    .to_string()
}
