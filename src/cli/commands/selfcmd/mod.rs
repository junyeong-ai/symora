//! `symora self` — binary lifecycle subcommands.

mod uninstall;
mod update;

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;

/// Best-effort daemon shutdown via the binary's own `daemon stop` command.
/// Used by both `self update` (before swapping the binary) and
/// `self uninstall` (before removing it). The result is intentionally
/// ignored — we never block lifecycle operations on daemon liveness.
pub(super) fn request_daemon_stop(exe: &Path) {
    if !exe.is_file() {
        return;
    }
    let _ = Command::new(exe)
        .args(["daemon", "stop"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status();
}

pub use uninstall::{UninstallArgs, UninstallOutcome, run_uninstall};
pub use update::{UpdateArgs, UpdateOutcome, run_update};

#[derive(Args, Debug)]
pub struct SelfcmdArgs {
    #[command(subcommand)]
    pub command: SelfcmdCommand,

    /// Skip all interactive prompts and accept defaults.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,
}

#[derive(Subcommand, Debug)]
pub enum SelfcmdCommand {
    /// Replace the running binary with a newer release.
    Update(UpdateArgs),
    /// Remove the binary, skill, config, and daemon runtime data.
    Uninstall(UninstallArgs),
}

#[derive(Serialize, Debug)]
struct SelfcmdOutput {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    update: Option<UpdateOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uninstall: Option<UninstallOutcome>,
}

pub async fn execute(args: SelfcmdArgs, app: &App) -> Result<()> {
    match args.command {
        SelfcmdCommand::Update(update_args) => {
            let outcome = run_update(update_args, args.yes)?;
            app.output.print_success(SelfcmdOutput {
                status: "ok".to_string(),
                update: Some(outcome),
                uninstall: None,
            });
        }
        SelfcmdCommand::Uninstall(uninstall_args) => {
            let outcome = run_uninstall(uninstall_args, args.yes)?;
            app.output.print_success(SelfcmdOutput {
                status: "ok".to_string(),
                update: None,
                uninstall: Some(outcome),
            });
        }
    }
    Ok(())
}
