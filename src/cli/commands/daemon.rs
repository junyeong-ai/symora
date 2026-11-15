use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::{OutputContext, OutputError};
use crate::daemon::client::Unaddressable;
use crate::daemon::wire::ProjectRestartStatus;
use crate::daemon::{DaemonClient, DaemonRuntimeConfig, DaemonServer, DaemonStart};
use crate::error::LspError;

fn start_message(outcome: DaemonStart) -> &'static str {
    match outcome {
        DaemonStart::AlreadyRunning => "Daemon is already running",
        DaemonStart::Started => "Daemon started",
        DaemonStart::Replaced => "Daemon from a different binary was replaced",
    }
}

/// What a daemon command answers where no daemon can serve this installation:
/// none can be running for it, so there is nothing to report, stop or unload,
/// and starting or serving one needs what `remedy` names. Every other command
/// already runs in process.
fn answer_unaddressable(command: DaemonCommand, reason: &Unaddressable, ctx: &OutputContext) {
    match command {
        DaemonCommand::Status => ctx.print_success(serde_json::json!({
            "running": false,
            "message": format!(
                "Daemon is not running: none can serve this installation, as {reason}; commands \
                 run in process"
            ),
        })),
        DaemonCommand::Stop => ctx.print_success(serde_json::json!({
            "stopped": false,
            "message": format!(
                "Daemon was not running: none can serve this installation, as {reason}"
            ),
        })),
        DaemonCommand::Restart { project: true } => ctx.print_success(serde_json::json!({
            "restarted": false,
            "message": format!(
                "Project was not loaded: no daemon can hold it, as {reason}; every command runs \
                 in process and starts its language servers with the current configuration"
            ),
        })),
        DaemonCommand::Start | DaemonCommand::Restart { project: false } | DaemonCommand::Serve => {
            let error = OutputError::precondition_failed(format!(
                "No daemon can run for this installation: {reason}"
            ))
            .with_hint(format!(
                "{} Until then every command runs in process with the current configuration \
                 and environment, answering the same without reusing language servers between \
                 invocations.",
                reason.remedy()
            ));
            ctx.print_error(error);
        }
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
    Restart {
        /// Unload only this project; start its servers on the next request
        #[arg(long)]
        project: bool,
    },

    /// Check daemon status
    Status,

    #[command(hide = true)]
    Serve,
}

async fn start_server() -> Result<()> {
    let config = DaemonRuntimeConfig::load()?;
    let server = DaemonServer::new(config);
    let run = server.run();
    tokio::pin!(run);
    let result = tokio::select! {
        result = &mut run => result,
        () = crate::infra::signal::stop_requested() => {
            server.shutdown();
            run.await
        }
    };

    if let Err(e) = result {
        tracing::error!("Daemon server error: {}", e);
        return Err(anyhow::anyhow!("Daemon server failed: {}", e));
    }

    Ok(())
}

pub async fn execute(args: DaemonArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let client = DaemonClient::new(app.root())?;
    if let Some(reason) = client.unaddressable() {
        answer_unaddressable(args.command, &reason, ctx);
        return Ok(());
    }

    match args.command {
        DaemonCommand::Start => {
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

        DaemonCommand::Restart { project: true } => {
            match client.restart_project().await {
                Ok(ProjectRestartStatus::Restarted) => ctx.print_success(serde_json::json!({
                    "restarted": true,
                    "message": "Project unloaded; language servers will start on the next request"
                })),
                Ok(ProjectRestartStatus::NotLoaded) => ctx.print_success(serde_json::json!({
                    "restarted": false,
                    "message": "Project was not loaded"
                })),
                Ok(ProjectRestartStatus::Busy) => ctx.print_error(
                    OutputError::conflict("Project has active requests or a language server is starting")
                        .with_hint("Wait for requests and server startup to finish, then retry `symora daemon restart --project`."),
                ),
                Err(e) => ctx.print_error(OutputError::from(e)),
            }
            Ok(())
        }

        DaemonCommand::Restart { project: false } => {
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

        DaemonCommand::Serve => start_server().await,

        DaemonCommand::Status => {
            match client.status().await {
                Ok(status) => {
                    ctx.print_success(status);
                }
                // Only a connect failure accepted by `proves_no_listener`
                // proves absence. Other failures leave the question open;
                // a definitive "not running" would be invented from them.
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
