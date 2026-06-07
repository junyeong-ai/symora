//! The host registry — the single place agent hosts are enumerated. Adding
//! a host is one `impl HostTarget` here plus one entry in `REGISTRY`;
//! nothing else in the installer changes.
//!
//! Current hosts both launch the MCP server at the active project's working
//! directory, so the entry needs no path override. Hosts that don't (e.g.
//! editors that spawn MCP subprocesses with a detached cwd) are deferred
//! until that behavior is handled explicitly, rather than shipped with a
//! workaround that could silently index the wrong tree.

use std::path::Path;

use anyhow::{Result, anyhow};

use super::host::{Env, FileAction, HostTarget, ServerSpec};
use super::writers::{Edit, json_remove, json_upsert, toml_remove, toml_upsert};
// Reuse the one audited, crash-safe file writer (O_EXCL staging + fsync +
// rename) rather than a second, weaker implementation — per the "no second
// file-writing implementation" rule in src/cli/CLAUDE.md.
use crate::cli::commands::edit::atomic_write;
use crate::services::dist::have;

/// Every supported host. Order is the deterministic output order.
const REGISTRY: &[&dyn HostTarget] = &[&ClaudeCode, &Codex];

pub fn all() -> &'static [&'static dyn HostTarget] {
    REGISTRY
}

pub fn find(id: &str) -> Option<&'static dyn HostTarget> {
    REGISTRY.iter().copied().find(|host| host.id() == id)
}

pub fn ids() -> Vec<&'static str> {
    REGISTRY.iter().map(|host| host.id()).collect()
}

// ---------------------------------------------------------------------------
// Claude Code — project-scoped `.mcp.json`, launched at the project root.
// ---------------------------------------------------------------------------

struct ClaudeCode;

impl HostTarget for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude_code"
    }

    fn config_path(&self, env: &Env) -> std::path::PathBuf {
        env.project_root.join(".mcp.json")
    }

    fn detect(&self, env: &Env) -> bool {
        env.home.join(".claude").is_dir() || have("claude")
    }

    fn install(&self, env: &Env, spec: &ServerSpec, dry_run: bool) -> Result<FileAction> {
        let path = self.config_path(env);
        let existing = read_existing(&path)?;
        let edit = json_upsert(existing.as_deref().unwrap_or_default(), "symora", spec)?;
        apply_upsert(&path, edit, existing.is_some(), dry_run)
    }

    fn uninstall(&self, env: &Env, dry_run: bool) -> Result<FileAction> {
        let path = self.config_path(env);
        let existing = read_existing(&path)?;
        let edit = json_remove(existing.as_deref().unwrap_or_default(), "symora")?;
        // The project `.mcp.json` is ours to own: if removing symora empties
        // it, delete the file so uninstall leaves no trace.
        apply_remove(&path, edit, DeleteWhenEmpty::Yes, dry_run)
    }
}

// ---------------------------------------------------------------------------
// Codex — user-scoped `~/.codex/config.toml`, launched at the workspace cwd.
// ---------------------------------------------------------------------------

struct Codex;

impl HostTarget for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn config_path(&self, env: &Env) -> std::path::PathBuf {
        env.home.join(".codex").join("config.toml")
    }

    fn detect(&self, env: &Env) -> bool {
        env.home.join(".codex").is_dir() || have("codex")
    }

    fn install(&self, env: &Env, spec: &ServerSpec, dry_run: bool) -> Result<FileAction> {
        let path = self.config_path(env);
        let existing = read_existing(&path)?;
        let edit = toml_upsert(existing.as_deref().unwrap_or_default(), "symora", spec)?;
        apply_upsert(&path, edit, existing.is_some(), dry_run)
    }

    fn uninstall(&self, env: &Env, dry_run: bool) -> Result<FileAction> {
        let path = self.config_path(env);
        let existing = read_existing(&path)?;
        let edit = toml_remove(existing.as_deref().unwrap_or_default(), "symora")?;
        // The user config is shared across projects and may hold unrelated
        // settings — only excise our table, never delete the file.
        apply_remove(&path, edit, DeleteWhenEmpty::No, dry_run)
    }
}

// ---------------------------------------------------------------------------
// Shared apply helpers — one write path, one action mapping for every host.
// ---------------------------------------------------------------------------

enum DeleteWhenEmpty {
    Yes,
    No,
}

/// Read an existing config, distinguishing "absent" (`None` — a fresh
/// install) from "present but unreadable" (`Err` — a permission or encoding
/// problem). The error propagates so the host is reported `Skipped` rather
/// than silently overwritten with a symora-only config.
fn read_existing(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("cannot read {}: {e}", path.display())),
    }
}

fn apply_upsert(path: &Path, edit: Edit, existed: bool, dry_run: bool) -> Result<FileAction> {
    if !edit.changed {
        return Ok(FileAction::Unchanged);
    }
    if !dry_run {
        // Creating the host's config directory (e.g. `~/.codex`) is the
        // installer's job; `atomic_write` only lands bytes into an existing
        // directory.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write(path, &edit.content)?;
    }
    Ok(if existed {
        FileAction::Updated
    } else {
        FileAction::Created
    })
}

fn apply_remove(
    path: &Path,
    edit: Edit,
    delete_when_empty: DeleteWhenEmpty,
    dry_run: bool,
) -> Result<FileAction> {
    if !edit.changed {
        return Ok(FileAction::NotFound);
    }
    if !dry_run {
        if matches!(delete_when_empty, DeleteWhenEmpty::Yes) && edit.now_empty {
            std::fs::remove_file(path)?;
        } else {
            atomic_write(path, &edit.content)?;
        }
    }
    Ok(FileAction::Removed)
}
