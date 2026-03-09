use anyhow::Result;
use clap::{Args, Subcommand};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::daemon::{DaemonClient, DaemonRuntimeConfig, DaemonServer};

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

fn spawn_server_process(root: &std::path::Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .current_dir(root)
        .arg("daemon")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

async fn wait_for_running(client: &DaemonClient, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(status) = client.status().await
            && status.get("running").and_then(|v| v.as_bool()) == Some(true)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

pub async fn execute(args: DaemonArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    match args.command {
        DaemonCommand::Start => {
            let client = DaemonClient::new(app.root());
            if let Ok(status) = client.status().await
                && status.get("running").and_then(|v| v.as_bool()) == Some(true)
            {
                ctx.print_success(serde_json::json!({
                    "started": false,
                    "message": "Daemon is already running"
                }));
                return Ok(());
            }

            spawn_server_process(app.root())?;
            let started = wait_for_running(&client, Duration::from_secs(5)).await;
            ctx.print_success(serde_json::json!({
                "started": started,
                "message": if started { "Daemon started" } else { "Daemon start requested" }
            }));
            Ok(())
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

            spawn_server_process(app.root())?;
            let restarted = wait_for_running(&client, Duration::from_secs(5)).await;
            ctx.print_success(serde_json::json!({
                "restarted": restarted,
                "message": if restarted { "Daemon restarted" } else { "Daemon restart requested" }
            }));
            Ok(())
        }

        DaemonCommand::Serve => start_server(app.root()).await,

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
