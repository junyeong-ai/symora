use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::app::App;
#[cfg(not(unix))]
use crate::cli::OutputError;
#[cfg(unix)]
use crate::daemon::DaemonClient;

use super::IndexCommand;

#[derive(Serialize, Deserialize)]
struct IndexStatusOutput {
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
    last_indexed: u64,
    #[serde(default)]
    is_indexing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<f32>,
}

#[derive(Serialize, Deserialize)]
struct IndexBuildOutput {
    status: String,
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
}

#[cfg(unix)]
#[derive(Deserialize)]
struct DaemonIndexBuildOutput {
    stats: DaemonIndexStats,
}

#[cfg(unix)]
#[derive(Deserialize)]
struct DaemonIndexStats {
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
}

pub async fn execute_index_command(app: &App, command: IndexCommand) -> Result<()> {
    let ctx = &app.output;

    #[cfg(unix)]
    {
        let client = DaemonClient::new(app.root());

        match command {
            IndexCommand::Build { force, languages } => {
                let langs: Option<Vec<String>> = languages.map(|s| {
                    s.split(',')
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                });

                match client.index_build(force, langs).await {
                    Ok(response) => {
                        let parsed: DaemonIndexBuildOutput = serde_json::from_value(response)
                            .map_err(|e| anyhow::anyhow!("Invalid daemon response: {}", e))?;
                        ctx.print_success(IndexBuildOutput {
                            status: "completed".to_string(),
                            file_count: parsed.stats.file_count,
                            symbol_count: parsed.stats.symbol_count,
                            content_line_count: parsed.stats.content_line_count,
                            index_size_bytes: parsed.stats.index_size_bytes,
                        });
                    }
                    Err(e) => {
                        ctx.print_error(e);
                    }
                }
            }
            IndexCommand::Status => match client.index_status().await {
                Ok(response) => {
                    let parsed: IndexStatusOutput = serde_json::from_value(response)
                        .map_err(|e| anyhow::anyhow!("Invalid daemon response: {}", e))?;
                    ctx.print_success(parsed);
                }
                Err(e) => {
                    ctx.print_error(e);
                }
            },
            IndexCommand::Clear => match client.index_clear().await {
                Ok(_) => {
                    ctx.print_success(serde_json::json!({ "cleared": true }));
                }
                Err(e) => {
                    ctx.print_error(e);
                }
            },
        }
    }

    #[cfg(not(unix))]
    {
        let _ = command;
        ctx.print_error(OutputError::unsupported(
            "Search index requires daemon mode (Unix only)",
        ));
    }

    Ok(())
}
