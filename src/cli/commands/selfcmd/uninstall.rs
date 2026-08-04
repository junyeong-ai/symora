//! `symora self uninstall` — remove the binary, skill, config, and daemon
//! runtime data, in that order. The binary's own path is the last thing to
//! go; on POSIX that's safe (the running process keeps its file descriptor
//! after `unlink`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::Args;
use serde::Serialize;

use crate::cli::utils::ui::{Step, confirm, section, step};
use crate::services::dist::paths;

#[derive(Args, Debug)]
pub struct UninstallArgs {
    /// Keep the user-level skill at `~/.claude/skills/symora`.
    #[arg(long)]
    pub keep_skill: bool,

    /// Keep the global config directory (`$XDG_CONFIG_HOME/symora` or `~/.config/symora`).
    #[arg(long)]
    pub keep_config: bool,

    /// Keep daemon runtime data (`~/.symora`).
    #[arg(long)]
    pub keep_daemon_data: bool,
}

#[derive(Serialize, Debug)]
pub struct UninstallOutcome {
    pub removed: Vec<RemovalRecord>,
    pub kept: Vec<&'static str>,
    pub binary: String,
}

#[derive(Serialize, Debug)]
pub struct RemovalRecord {
    pub kind: RemovalKind,
    pub path: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum RemovalKind {
    Skill,
    Config,
    DaemonData,
    Binary,
    EmptyParent,
}

pub fn run_uninstall(args: UninstallArgs, assume_yes: bool) -> Result<UninstallOutcome> {
    section("self uninstall");

    let exe_raw = std::env::current_exe().context("could not determine current binary path")?;
    // canonicalize follows symlinks (e.g. mise/asdf shims) so we delete the
    // real file. If it fails, fall back to the raw path — better to remove
    // the symlink than to abort.
    let exe = exe_raw.canonicalize().unwrap_or(exe_raw);

    // Default to yes — the user explicitly invoked `uninstall`. The prompt is
    // a sanity check, not a gate. With `--yes` the prompt is skipped entirely
    // and the default is taken; an interactive user can still type `n` to abort.
    if !confirm(
        &format!("Remove symora and its data? (binary at {})", exe.display()),
        true,
        assume_yes,
    ) {
        return Err(anyhow!("uninstall cancelled"));
    }

    if exe.is_file() {
        step(Step::Run, "stopping daemon (best effort)");
        super::request_daemon_stop(&exe);
    }

    let mut removed: Vec<RemovalRecord> = Vec::new();
    let mut kept: Vec<&'static str> = Vec::new();

    remove_skill(&args, &mut removed, &mut kept)?;
    remove_simple_dir(
        paths::config_dir(),
        RemovalKind::Config,
        "config",
        args.keep_config,
        &mut removed,
        &mut kept,
    )?;
    remove_simple_dir(
        paths::daemon_dir(),
        RemovalKind::DaemonData,
        "daemon_data",
        args.keep_daemon_data,
        &mut removed,
        &mut kept,
    )?;
    remove_binary(&exe, &mut removed);

    Ok(UninstallOutcome {
        removed,
        kept,
        binary: exe.display().to_string(),
    })
}

fn remove_skill(
    args: &UninstallArgs,
    removed: &mut Vec<RemovalRecord>,
    kept: &mut Vec<&'static str>,
) -> Result<()> {
    let skill = paths::skill_dir()?;
    if args.keep_skill {
        kept.push("skill");
        step(
            Step::Skip,
            format!("kept skill ({})", paths::display(&skill)),
        );
        return Ok(());
    }
    if !skill.is_dir() {
        step(Step::Skip, "no user-level skill present");
        return Ok(());
    }

    std::fs::remove_dir_all(&skill).with_context(|| format!("removing {}", skill.display()))?;
    step(
        Step::Ok,
        format!("removed skill ({})", paths::display(&skill)),
    );
    removed.push(RemovalRecord {
        kind: RemovalKind::Skill,
        path: skill.display().to_string(),
    });

    prune_empty_ancestors(&skill, removed, 2);
    Ok(())
}

fn remove_simple_dir(
    dir: PathBuf,
    kind: RemovalKind,
    label: &'static str,
    keep: bool,
    removed: &mut Vec<RemovalRecord>,
    kept: &mut Vec<&'static str>,
) -> Result<()> {
    if keep {
        kept.push(label);
        step(
            Step::Skip,
            format!("kept {label} ({})", paths::display(&dir)),
        );
        return Ok(());
    }
    if !dir.is_dir() {
        step(Step::Skip, format!("no {label} directory present"));
        return Ok(());
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    step(
        Step::Ok,
        format!("removed {label} ({})", paths::display(&dir)),
    );
    removed.push(RemovalRecord {
        kind,
        path: dir.display().to_string(),
    });
    Ok(())
}

fn remove_binary(exe: &Path, removed: &mut Vec<RemovalRecord>) {
    if !exe.is_file() {
        step(Step::Warn, format!("binary not found at {}", exe.display()));
        return;
    }
    if let Err(err) = std::fs::remove_file(exe) {
        step(
            Step::Warn,
            format!("could not remove binary {}: {err}", exe.display()),
        );
        return;
    }
    step(Step::Ok, format!("removed binary ({})", exe.display()));
    removed.push(RemovalRecord {
        kind: RemovalKind::Binary,
        path: exe.display().to_string(),
    });
}

fn prune_empty_ancestors(start: &Path, removed: &mut Vec<RemovalRecord>, levels: usize) {
    let mut current = start.parent().map(|p| p.to_path_buf());
    for _ in 0..levels {
        let Some(dir) = current else {
            break;
        };
        if !remove_if_empty(&dir) {
            break;
        }
        removed.push(RemovalRecord {
            kind: RemovalKind::EmptyParent,
            path: dir.display().to_string(),
        });
        current = dir.parent().map(|p| p.to_path_buf());
    }
}

fn remove_if_empty(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    match std::fs::read_dir(dir) {
        Ok(mut iter) => {
            if iter.next().is_some() {
                return false;
            }
        }
        Err(_) => return false,
    }
    std::fs::remove_dir(dir).is_ok()
}
