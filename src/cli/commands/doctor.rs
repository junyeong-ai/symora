use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::infra::lsp::servers::{Platform, check_all_servers};

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
}

#[derive(Serialize)]
struct LanguageStatus {
    language: String,
    server: String,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    tier: String,
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
    let all_servers = check_all_servers();

    let languages: Vec<LanguageStatus> = all_servers
        .into_iter()
        .filter(|s| {
            args.language
                .as_ref()
                .is_none_or(|filter| s.language.to_string().contains(&filter.to_lowercase()))
        })
        .map(|s| LanguageStatus {
            language: s.language.to_string(),
            server: s.name.to_string(),
            installed: s.installed,
            version: s.version,
            tier: s.tier.as_str().to_string(),
            install: if s.installed {
                None
            } else {
                Some(s.install_instruction.to_string())
            },
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
