//! The host-target abstraction: one implementation per agent that hosts an
//! MCP server. Adding a host is a single `impl HostTarget` plus one line in
//! the registry — the trait localizes every host-specific fact (config
//! location, file format, install signal) so nothing else in the installer
//! changes.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::cli::OutputError;

/// Resolved filesystem context, injected once so the whole installer is
/// hermetic against a temporary `HOME`/project in tests rather than reading
/// the real environment from inside each host.
pub struct Env {
    pub home: PathBuf,
    pub project_root: PathBuf,
}

/// The MCP-server invocation a host writes, in host-neutral form. Each host
/// serializes it into its own config format.
pub struct ServerSpec {
    /// Absolute path to the running `symora` binary.
    pub command: String,
    pub args: Vec<String>,
}

/// What happened to a host's config file.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileAction {
    /// The config file did not exist and was created.
    Created,
    /// An existing config gained or changed the `symora` entry.
    Updated,
    /// The `symora` entry already matched — nothing was written.
    Unchanged,
    /// The `symora` entry was removed.
    Removed,
    /// Uninstall found no `symora` entry to remove.
    NotFound,
    /// The host's config could not be parsed or written; it was left
    /// untouched. `error` carries the reason.
    Skipped,
}

/// Per-host result, surfaced as one item of the response list.
#[derive(Debug, Serialize)]
pub struct HostOutcome {
    pub host: &'static str,
    pub action: FileAction,
    /// The config file acted on, shown relative to `$HOME` when possible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// Whether the host was detected as installed. Surfaced so an agent can
    /// see why an explicitly-requested host was a no-op.
    pub detected: bool,
    /// Set when the host was skipped because its config could not be parsed
    /// or written — the original file is left untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OutputError>,
}

/// One agent host that can run the `symora mcp serve` MCP server.
pub trait HostTarget: Sync {
    /// Stable identifier used by `--host` selection and in output.
    fn id(&self) -> &'static str;

    /// Absolute path to the config file this host reads.
    fn config_path(&self, env: &Env) -> PathBuf;

    /// Whether this host is actually installed. Auto-detection only writes
    /// where this is true; it must be a positive signal the host itself
    /// produced (its own directory, or its binary on `$PATH`) — never the
    /// config file the installer manages, which would be circular.
    fn detect(&self, env: &Env) -> bool;

    /// Install or update the `symora` MCP-server entry. Idempotent: an
    /// entry that already matches yields `Unchanged` and writes nothing.
    fn install(&self, env: &Env, spec: &ServerSpec, dry_run: bool) -> Result<FileAction>;

    /// Remove the `symora` entry, reversing `install`. Yields `NotFound`
    /// when there was nothing to remove.
    fn uninstall(&self, env: &Env, dry_run: bool) -> Result<FileAction>;
}
