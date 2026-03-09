use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::daemon::{DaemonClient, DaemonRuntimeConfig, DaemonServer};

#[derive(Args, Debug)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start the daemon server (runs in foreground)
    Start,

    /// Stop the running daemon
    Stop,

    /// Restart the daemon (stop + start)
    Restart,

    /// Check daemon status
    Status,
}

async fn start_server(root: &std::path::Path) -> Result<()> {
    let config = DaemonRuntimeConfig::load(root);
    let lsp_config = DaemonRuntimeConfig::load_lsp_config(root);
    let server = DaemonServer::new(config, lsp_config);

    if let Err(e) = server.run().await {
        tracing::error!("Daemon server error: {}", e);
        return Err(anyhow::anyhow!("Daemon server failed: {}", e));
    }

    Ok(())
}

pub async fn execute(args: DaemonArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    match args.command {
        DaemonCommand::Start => {
            tracing::info!("Starting daemon server...");
            start_server(app.root()).await
        }

        DaemonCommand::Stop => {
            let client = DaemonClient::new(app.root());

            match client.shutdown().await {
                Ok(_) => {
                    ctx.print_success(serde_json::json!({
                        "stopped": true,
                        "message": "Daemon shutdown signal sent"
                    }));
                }
                Err(_) => {
                    ctx.print_success(serde_json::json!({
                        "stopped": false,
                        "message": "Daemon was not running"
                    }));
                }
            }
            Ok(())
        }

        DaemonCommand::Restart => {
            let client = DaemonClient::new(app.root());

            // Stop existing daemon
            let _ = client.shutdown().await;

            // Wait a moment for clean shutdown
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            tracing::info!("Restarting daemon server...");
            start_server(app.root()).await
        }

        DaemonCommand::Status => {
            let client = DaemonClient::new(app.root());

            match client.status().await {
                Ok(status) => {
                    ctx.print_success(status);
                }
                Err(_) => {
                    ctx.print_success(serde_json::json!({
                        "running": false,
                        "message": "Daemon is not running"
                    }));
                }
            }
            Ok(())
        }
    }
}
