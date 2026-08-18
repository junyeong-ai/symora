use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::OutputError;
use crate::daemon::{DaemonClient, DaemonRuntimeConfig, DaemonServer, DaemonStart};
use crate::error::LspError;

fn start_message(outcome: DaemonStart) -> &'static str {
    match outcome {
        DaemonStart::AlreadyRunning => "Daemon is already running",
        DaemonStart::Started => "Daemon started",
        DaemonStart::Replaced => "Daemon from a different binary was replaced",
    }
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start the daemon server in the background
    Start,

    /// Stop the running daemon
    Stop,

    /// Restart the daemon in the background
    Restart,

    /// Check daemon status
    Status,

    #[command(hide = true)]
    Serve,
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
            let client = DaemonClient::new(app.root());
            match client.ensure_running().await {
                Ok(outcome) => ctx.print_success(serde_json::json!({
                    "started": outcome != DaemonStart::AlreadyRunning,
                    "message": start_message(outcome),
                })),
                Err(e) => ctx.print_error(OutputError::from(e)),
            }
            Ok(())
        }

        DaemonCommand::Stop => {
            let client = DaemonClient::new(app.root());

            match client.shutdown().await {
                Ok(true) => ctx.print_success(serde_json::json!({
                    "stopped": true,
                    "message": "Daemon stopped"
                })),
                Ok(false) => ctx.print_success(serde_json::json!({
                    "stopped": false,
                    "message": "Daemon was not running"
                })),
                Err(e) => ctx.print_error(OutputError::from(e)),
            }
            Ok(())
        }

        DaemonCommand::Restart => {
            let client = DaemonClient::new(app.root());
            let restarted = match client.shutdown().await {
                Ok(_) => client.ensure_running().await.map(|_| ()),
                Err(e) => Err(e),
            };

            match restarted {
                Ok(()) => ctx.print_success(serde_json::json!({
                    "restarted": true,
                    "message": "Daemon restarted"
                })),
                Err(e) => ctx.print_error(OutputError::from(e)),
            }
            Ok(())
        }

        DaemonCommand::Serve => start_server(app.root()).await,

        DaemonCommand::Status => {
            let client = DaemonClient::new(app.root());

            match client.status().await {
                Ok(status) => {
                    ctx.print_success(status);
                }
                // Only a refused or absent socket proves there is no daemon.
                // Any other failure left the question unanswered, and a
                // definitive "not running" would be invented from it.
                Err(LspError::NotConnected) => {
                    ctx.print_success(serde_json::json!({
                        "running": false,
                        "message": "Daemon is not running"
                    }));
                }
                Err(e) => ctx.print_error(OutputError::from(e)),
            }
            Ok(())
        }
    }
}
