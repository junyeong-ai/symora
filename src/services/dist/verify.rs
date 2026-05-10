//! SHA-256 + GitHub attestation verification, both via shell-out.

use std::path::Path;

use anyhow::{Context, Result, anyhow};

use super::process::{have, run_streaming, run_streaming_in};
use super::release::REPO;

/// Verify a downloaded archive against its sidecar `.sha256` file. The
/// sidecar must already be in the same directory as the archive.
pub fn verify_sha256(archive: &Path) -> Result<()> {
    let dir = archive
        .parent()
        .ok_or_else(|| anyhow!("archive has no parent directory: {}", archive.display()))?;
    let archive_name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("non-UTF-8 archive name: {}", archive.display()))?;
    let sidecar = format!("{archive_name}.sha256");

    if have("sha256sum") {
        run_streaming_in("sha256sum", &["-c", &sidecar], Some(dir))
    } else if have("shasum") {
        run_streaming_in("shasum", &["-a", "256", "-c", &sidecar], Some(dir))
    } else {
        return Err(anyhow!(
            "neither sha256sum nor shasum is available — refusing to install without verification"
        ));
    }
    .with_context(|| format!("checksum verification failed for {archive_name}"))
}

/// Verify GitHub build provenance with the `gh` CLI. Hard-required when the
/// caller asks for it — this is opt-in security, so a missing `gh` or a
/// failed attestation must abort the install.
pub fn verify_attestation(archive: &Path) -> Result<()> {
    if !have("gh") {
        return Err(anyhow!(
            "attestation verification requires the 'gh' CLI (https://cli.github.com)"
        ));
    }
    let archive_str = archive
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 archive path"))?;
    run_streaming(
        "gh",
        &["attestation", "verify", archive_str, "--repo", REPO],
    )
    .with_context(|| {
        format!(
            "attestation verification failed for {}",
            archive
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("archive")
        )
    })
}
