//! Daemon management command implementation
//!
//! Start, stop, and check status of the daemon server.

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::daemon::{DaemonClient, DaemonConfig, DaemonServer};

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

pub async fn execute(args: DaemonArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    match args.command {
        DaemonCommand::Start => {
            // Initialize LSP settings from config before starting
            DaemonConfig::init_lsp_settings();

            let config = DaemonConfig::default();
            let server = DaemonServer::new(config);

            tracing::info!("Starting daemon server...");

            if let Err(e) = server.run().await {
                tracing::error!("Daemon server error: {}", e);
                return Err(anyhow::anyhow!("Daemon server failed: {}", e));
            }

            Ok(())
        }

        DaemonCommand::Stop => {
            let client = DaemonClient::new(app.root());

            match client.shutdown().await {
                Ok(_) => {
                    ctx.print_success_flat(serde_json::json!({
                        "stopped": true,
                        "message": "Daemon shutdown signal sent"
                    }));
                }
                Err(_) => {
                    ctx.print_success_flat(serde_json::json!({
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

            // Initialize LSP settings from config before starting
            DaemonConfig::init_lsp_settings();

            // Start new daemon
            let config = DaemonConfig::default();
            let server = DaemonServer::new(config);

            tracing::info!("Restarting daemon server...");

            if let Err(e) = server.run().await {
                tracing::error!("Daemon server error: {}", e);
                return Err(anyhow::anyhow!("Daemon server failed: {}", e));
            }

            Ok(())
        }

        DaemonCommand::Status => {
            let client = DaemonClient::new(app.root());

            match client.status().await {
                Ok(status) => {
                    ctx.print_success_flat(status);
                }
                Err(_) => {
                    ctx.print_success_flat(serde_json::json!({
                        "running": false,
                        "message": "Daemon is not running"
                    }));
                }
            }
            Ok(())
        }
    }
}
