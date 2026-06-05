//! Multi-repo workspace support.
//!
//! A workspace is a TOML file at `~/.config/symora/workspaces/<name>.toml`
//! that names a set of project roots. With `--workspace <name>`, the
//! Symora CLI fans the same command out across every root and packs the
//! per-root JSON outputs into one response, so an agent can run a single
//! `pack` / `map` / `search` call across a microservice fleet.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::constants::env::FORMAT_OVERRIDE;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("Workspace '{0}' not found at {1}")]
    NotFound(String, PathBuf),
    #[error("Workspace '{name}' has no roots configured")]
    Empty { name: String },
    #[error("Workspace config parse error: {0}")]
    Parse(String),
    #[error("Could not resolve home directory")]
    NoHomeDir,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub name: String,
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RawWorkspace {
    #[serde(default)]
    name: Option<String>,
    roots: Vec<PathBuf>,
}

impl WorkspaceConfig {
    pub fn load(name: &str) -> Result<Self, WorkspaceError> {
        let path = workspace_path(name)?;
        if !path.exists() {
            return Err(WorkspaceError::NotFound(name.to_string(), path));
        }
        let content = std::fs::read_to_string(&path)?;
        let raw: RawWorkspace =
            toml::from_str(&content).map_err(|e| WorkspaceError::Parse(e.to_string()))?;
        let resolved_name = raw.name.unwrap_or_else(|| name.to_string());

        let roots: Vec<PathBuf> = raw.roots.into_iter().map(expand_tilde).collect();
        if roots.is_empty() {
            return Err(WorkspaceError::Empty {
                name: resolved_name,
            });
        }

        Ok(Self {
            name: resolved_name,
            roots,
        })
    }
}

fn workspace_path(name: &str) -> Result<PathBuf, WorkspaceError> {
    // Stay XDG-compliant on every platform — Symora's main config lives at
    // ~/.config/symora/config.toml, and workspaces sit alongside it. macOS
    // dirs::config_dir() points at ~/Library/Application Support/, which
    // would surprise anyone composing with the rest of the CLI.
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .ok_or(WorkspaceError::NoHomeDir)?;
    Ok(base
        .join("symora")
        .join("workspaces")
        .join(format!("{name}.toml")))
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    if s == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    path
}

/// Strip `--workspace VALUE` and `--workspace=VALUE` from a raw argv,
/// preserving everything else. Used by the workspace runner so spawned
/// child processes don't recurse.
pub fn strip_workspace_flag(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--workspace" {
            iter.next();
            continue;
        }
        if arg.starts_with("--workspace=") {
            continue;
        }
        out.push(arg);
    }
    out
}

/// Run one Symora invocation against each root in the workspace and
/// bundle the per-root JSON responses into a single envelope.
///
/// Roots run in parallel (bounded by `WORKSPACE_FANOUT`) — a 10-repo
/// workspace finishes in `max(per-repo)` instead of `sum(per-repo)`.
/// Results are collected back in declared root order so JSON consumers
/// can index by position deterministically.
pub async fn run_workspace(
    ws: WorkspaceConfig,
    exe: &Path,
    forwarded_args: Vec<String>,
) -> serde_json::Value {
    use futures::stream::{self, StreamExt};

    let args = std::sync::Arc::new(forwarded_args);
    let exe = exe.to_path_buf();

    // Capped concurrency: prevents a 100-root workspace from forking
    // 100 daemons at once. 4 is a reasonable default for read-heavy
    // commands; mutating commands run effectively serially because each
    // root is its own filesystem.
    const WORKSPACE_FANOUT: usize = 4;

    let entries: Vec<(usize, serde_json::Value)> = stream::iter(ws.roots.iter().enumerate())
        .map(|(idx, root)| {
            let exe = exe.clone();
            let args = args.clone();
            async move { (idx, run_one_root(&exe, root, &args).await) }
        })
        .buffer_unordered(WORKSPACE_FANOUT)
        .collect()
        .await;

    let mut sorted = entries;
    sorted.sort_by_key(|(idx, _)| *idx);
    let results: Vec<serde_json::Value> = sorted.into_iter().map(|(_, v)| v).collect();

    serde_json::json!({
        "workspace": ws.name,
        "roots": ws.roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "items": results,
    })
}

async fn run_one_root(exe: &Path, root: &Path, args: &[String]) -> serde_json::Value {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args);
    cmd.current_dir(root);
    // Force compact JSON inside the child so the envelope stays
    // single-line for the host process to embed.
    cmd.env(FORMAT_OVERRIDE, "compact");

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            return serde_json::json!({
                "root": root.display().to_string(),
                "error": format!("Failed to spawn child: {e}"),
            });
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed = serde_json::from_str::<serde_json::Value>(stdout.trim())
        .unwrap_or_else(|_| serde_json::Value::String(stdout.into_owned()));

    let exit = output.status.code();
    let mut entry = serde_json::json!({
        "root": root.display().to_string(),
        "exit": exit,
        "data": parsed,
    });
    if !output.status.success() {
        entry["error"] =
            serde_json::Value::String(format!("Child exited with non-zero status ({:?})", exit));
    }
    if !stderr.trim().is_empty() {
        entry["stderr"] = serde_json::Value::String(stderr.into_owned());
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_workspace_flag_handles_split_form() {
        let args = vec![
            "symora".into(),
            "--workspace".into(),
            "platform".into(),
            "pack".into(),
            "--tokens".into(),
            "1000".into(),
        ];
        assert_eq!(
            strip_workspace_flag(args),
            vec!["symora", "pack", "--tokens", "1000"],
        );
    }

    #[test]
    fn strip_workspace_flag_handles_equals_form() {
        let args = vec![
            "symora".into(),
            "--workspace=platform".into(),
            "pack".into(),
        ];
        assert_eq!(strip_workspace_flag(args), vec!["symora", "pack"],);
    }

    #[test]
    fn strip_workspace_flag_is_noop_without_flag() {
        let args = vec![
            "symora".into(),
            "pack".into(),
            "--tokens".into(),
            "1000".into(),
        ];
        assert_eq!(strip_workspace_flag(args.clone()), args);
    }

    #[test]
    fn expand_tilde_replaces_home_prefix() {
        if let Some(home) = dirs::home_dir() {
            let expanded = expand_tilde(PathBuf::from("~/projects/foo"));
            assert!(expanded.starts_with(&home));
            assert!(expanded.ends_with("projects/foo"));
        }
    }

    #[test]
    fn expand_tilde_leaves_absolute_paths_alone() {
        let p = PathBuf::from("/abs/path");
        assert_eq!(expand_tilde(p.clone()), p);
    }

    #[test]
    fn load_returns_not_found_for_missing_workspace() {
        let result = WorkspaceConfig::load("__symora_definitely_does_not_exist__");
        assert!(matches!(result, Err(WorkspaceError::NotFound(_, _))));
    }
}
