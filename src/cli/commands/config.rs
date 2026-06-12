use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
use crate::models::config::SymoraConfig;

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Initialize configuration file
    Init {
        /// Initialize global config (~/.config/symora)
        #[arg(long)]
        global: bool,

        /// Force overwrite existing config
        #[arg(short, long)]
        force: bool,
    },

    /// Show current configuration
    Show {
        /// Show global config only
        #[arg(long)]
        global: bool,
    },

    /// Show config file path
    Path {
        /// Show global config path
        #[arg(long)]
        global: bool,
    },

    /// Edit configuration with default editor
    Edit {
        /// Edit global config
        #[arg(long)]
        global: bool,
    },
}

#[derive(Serialize)]
struct ConfigInitOutput {
    status: String,
    path: String,
    level: &'static str,
}

#[derive(Serialize)]
struct ConfigShowOutput {
    level: &'static str,
    config: serde_json::Value,
    /// Config problems affecting this view: rejected [lsp.servers]
    /// stanzas — non-canonical keys, unknown fields, mistyped values —
    /// recorded at load and never applied. Same field name and presence
    /// rule as `doctor`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    config_errors: Vec<String>,
}

#[derive(Serialize)]
struct ConfigPathOutput {
    level: &'static str,
    path: String,
    exists: bool,
}

#[derive(Serialize)]
struct ConfigEditOutput {
    status: String,
    path: String,
}

fn config_to_json(config: &SymoraConfig) -> serde_json::Value {
    serde_json::json!({
        "project": {
            "name": config.project.name,
            "languages": config.project.languages.iter()
                .map(|l| l.lsp_id())
                .collect::<Vec<_>>(),
            "ignored_paths": config.project.ignored_paths,
        },
        "lsp": {
            "timeout_secs": config.lsp.timeout_secs,
            "auto_restart": config.lsp.auto_restart,
            "refs_limit": config.lsp.refs_limit,
            "impl_limit": config.lsp.impl_limit,
            "symbol_limit": config.lsp.symbol_limit,
            "calls_limit": config.lsp.calls_limit,
            "type_hierarchy_limit": config.lsp.type_hierarchy_limit,
            "tests_limit": config.lsp.tests_limit,
            "servers": config.lsp.servers,
        },
        "search": {
            "limit": config.search.limit,
            "max_file_size_mb": config.search.max_file_size_mb,
        },
        "daemon": {
            "max_concurrent": config.daemon.max_concurrent,
            "idle_timeout_mins": config.daemon.idle_timeout_mins,
        },
        "output": {
            "max_response_chars": config.output.max_response_chars,
        },
    })
}

pub async fn execute(args: ConfigArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    match args.command {
        ConfigCommand::Init { global, force } => {
            let level = if global { "global" } else { "project" };
            match app.config_service.init(global, force).await {
                Ok(path) => {
                    let response = ConfigInitOutput {
                        status: "created".to_string(),
                        path: if global {
                            path.display().to_string()
                        } else {
                            ctx.relative_path(&path)
                        },
                        level,
                    };
                    ctx.print_success(response);
                }
                Err(e) => ctx.print_error(e),
            }
        }

        ConfigCommand::Show { global } => {
            let level = if global { "global" } else { "merged" };
            match app.config_service.load(global).await {
                Ok(config) => {
                    let response = ConfigShowOutput {
                        level,
                        config_errors: config
                            .lsp
                            .server_override_errors
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                        config: config_to_json(&config),
                    };
                    ctx.print_success(response);
                }
                Err(e) => ctx.print_error(e),
            }
        }

        ConfigCommand::Path { global } => {
            let level = if global { "global" } else { "project" };
            let path = app.config_service.config_path(global);
            let response = ConfigPathOutput {
                level,
                path: if global {
                    path.display().to_string()
                } else {
                    ctx.relative_path(&path)
                },
                exists: path.exists(),
            };
            ctx.print_success(response);
        }

        ConfigCommand::Edit { global } => match app.config_service.edit(global).await {
            Ok(path) => {
                let response = ConfigEditOutput {
                    status: "opened".to_string(),
                    path: if global {
                        path.display().to_string()
                    } else {
                        ctx.relative_path(&path)
                    },
                };
                ctx.print_success(response);
            }
            Err(e) => ctx.print_error(e),
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `config_errors` follows doctor's presence rule: omitted entirely on
    /// a clean config, an array of strings when stanzas were rejected.
    #[test]
    fn config_show_output_omits_config_errors_when_clean() {
        let clean = ConfigShowOutput {
            level: "merged",
            config: serde_json::json!({}),
            config_errors: Vec::new(),
        };
        let value = serde_json::to_value(&clean).unwrap();
        assert!(value.get("config_errors").is_none());

        let rejected = ConfigShowOutput {
            level: "merged",
            config: serde_json::json!({}),
            config_errors: vec![
                "Invalid value for 'lsp.servers.klingon': unknown language — \
                 use the `language` id shown by `symora doctor`"
                    .to_string(),
            ],
        };
        let value = serde_json::to_value(&rejected).unwrap();
        assert_eq!(value["config_errors"].as_array().unwrap().len(), 1);
    }
}
