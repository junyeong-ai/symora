//! `symora self update` — atomic self-replacement from a GitHub release.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use clap::Args;
use serde::Serialize;

use crate::cli::utils::ui::{Step, confirm, section, step};
use crate::services::dist::{
    TempDir, current_target, download_release, extract_symora_archive, is_valid_version,
    resolve_latest_version, verify_attestation, verify_sha256,
};

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Specific version to install (e.g. `0.7.0`). Defaults to the latest release.
    #[arg(long, value_name = "VER")]
    pub version: Option<String>,

    /// Skip the version-equal short-circuit and reinstall.
    #[arg(long)]
    pub force: bool,

    /// Verify GitHub build provenance with the `gh` CLI.
    #[arg(long)]
    pub verify_attestations: bool,
}

#[derive(Serialize, Debug)]
pub struct UpdateOutcome {
    pub action: UpdateAction,
    pub from_version: String,
    pub to_version: String,
    pub target: String,
    pub binary: String,
    pub attestation_verified: bool,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    Updated,
    Reinstalled,
    AlreadyCurrent,
    Cancelled,
}

pub fn run_update(args: UpdateArgs, assume_yes: bool) -> Result<UpdateOutcome> {
    section("self update");

    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let target_version = match args.version.as_deref().map(strip_v) {
        Some(v) => {
            if !is_valid_version(v) {
                return Err(anyhow!("invalid version: {v}"));
            }
            v.to_string()
        }
        None => {
            step(Step::Run, "resolving latest release");
            resolve_latest_version()?
        }
    };

    let target = current_target()?;
    let exe = std::env::current_exe().context("could not determine current binary path")?;
    // canonicalize follows symlinks (e.g. mise/asdf shims) so the rename hits
    // the real file. If it fails, fall back to the raw path.
    let exe_canonical = exe.canonicalize().unwrap_or(exe);

    step(
        Step::Info,
        format!("running v{current_version} → target v{target_version} ({target})"),
    );

    if !args.force && current_version == target_version {
        step(Step::Skip, "already at requested version");
        return Ok(UpdateOutcome {
            action: UpdateAction::AlreadyCurrent,
            from_version: current_version,
            to_version: target_version,
            target: target.to_string(),
            binary: exe_canonical.display().to_string(),
            attestation_verified: false,
        });
    }

    if !confirm(
        &format!("Replace the running binary with v{target_version}?"),
        true,
        assume_yes,
    ) {
        step(Step::Skip, "cancelled");
        return Ok(UpdateOutcome {
            action: UpdateAction::Cancelled,
            from_version: current_version,
            to_version: target_version,
            target: target.to_string(),
            binary: exe_canonical.display().to_string(),
            attestation_verified: false,
        });
    }

    let workspace = TempDir::new("symora-self-update")?;

    step(Step::Run, "downloading release");
    let asset = download_release(&target_version, target, workspace.path())?;
    step(
        Step::Ok,
        format!(
            "downloaded {}",
            asset
                .archive
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("archive")
        ),
    );

    step(Step::Run, "verifying SHA-256");
    verify_sha256(&asset.archive)?;
    step(Step::Ok, "checksum OK");

    let attestation_verified = if args.verify_attestations {
        step(Step::Run, "verifying GitHub attestation");
        verify_attestation(&asset.archive)?;
        step(Step::Ok, "attestation OK");
        true
    } else {
        false
    };

    let extract_root = workspace.path().join("extract");
    std::fs::create_dir_all(&extract_root)?;
    let new_binary = extract_symora_archive(&asset.archive, &extract_root)?;

    super::request_daemon_stop(&exe_canonical);

    atomic_replace(&new_binary, &exe_canonical)?;
    macos_codesign(&exe_canonical);

    let action = if current_version == target_version {
        UpdateAction::Reinstalled
    } else {
        UpdateAction::Updated
    };

    step(
        Step::Ok,
        format!("binary replaced at {}", exe_canonical.display()),
    );

    Ok(UpdateOutcome {
        action,
        from_version: current_version,
        to_version: target_version,
        target: target.to_string(),
        binary: exe_canonical.display().to_string(),
        attestation_verified,
    })
}

fn atomic_replace(new_bin: &Path, dest: &Path) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent: {}", dest.display()))?;
    let staging = parent.join(format!(
        ".{}.symora-update.{}",
        dest.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("symora"),
        std::process::id()
    ));

    let mut guard = StagingFile::new(&staging);
    std::fs::copy(new_bin, &staging)
        .with_context(|| format!("staging new binary at {}", staging.display()))?;
    set_executable(&staging)?;
    std::fs::rename(&staging, dest)
        .with_context(|| format!("atomic rename {} -> {}", staging.display(), dest.display()))?;
    guard.disarm();
    Ok(())
}

/// RAII cleanup for the staging file. Removes the staged copy on Drop unless
/// `disarm()` has been called — used to clean up after a partial copy or
/// failed rename without leaving an orphan dotfile next to the binary.
struct StagingFile {
    path: std::path::PathBuf,
    armed: bool,
}

impl StagingFile {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn set_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(perm.mode() | 0o755);
    std::fs::set_permissions(p, perm)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_p: &Path) -> Result<()> {
    Ok(())
}

fn macos_codesign(p: &Path) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let _ = std::process::Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(p)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn strip_v(v: &str) -> &str {
    v.strip_prefix('v').unwrap_or(v)
}
