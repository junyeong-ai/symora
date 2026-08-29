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

/// Whether provenance can and must be checked for this install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttestationPolicy {
    Verify,
    Skip,
    ToolMissing,
    BundleMissing,
}

/// Provenance is checked whenever `gh` can check it. Only a missing `gh`
/// skips the step, and `--verify-attestations` refuses even that. A release
/// whose bundle cannot be fetched is refused rather than installed unchecked:
/// the checksum beside the archive is published by whoever published the
/// archive.
pub fn attestation_policy(
    gh_installed: bool,
    bundle_available: bool,
    required: bool,
) -> AttestationPolicy {
    match (gh_installed, bundle_available, required) {
        (false, _, true) => AttestationPolicy::ToolMissing,
        (false, _, false) => AttestationPolicy::Skip,
        (true, true, _) => AttestationPolicy::Verify,
        (true, false, _) => AttestationPolicy::BundleMissing,
    }
}

/// Verify GitHub build provenance against the bundle published with the
/// release. Reading the bundle from disk keeps this offline: the attestations
/// API needs an authenticated `gh`, a file does not. The signer is pinned to
/// this repository's release workflow.
pub fn verify_attestation(archive: &Path, bundle: &Path) -> Result<()> {
    let archive_str = archive
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 archive path"))?;
    let bundle_str = bundle
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 bundle path"))?;
    let signer = format!("{REPO}/.github/workflows/release.yml");
    run_streaming(
        "gh",
        &[
            "attestation",
            "verify",
            archive_str,
            "--bundle",
            bundle_str,
            "--repo",
            REPO,
            "--signer-workflow",
            &signer,
        ],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_is_checked_whenever_gh_can_check_it() {
        assert_eq!(
            attestation_policy(true, true, false),
            AttestationPolicy::Verify
        );
        assert_eq!(
            attestation_policy(true, true, true),
            AttestationPolicy::Verify
        );
    }

    #[test]
    fn only_a_missing_gh_skips_the_check() {
        assert_eq!(
            attestation_policy(false, true, false),
            AttestationPolicy::Skip
        );
        assert_eq!(
            attestation_policy(false, false, false),
            AttestationPolicy::Skip
        );
        assert_eq!(
            attestation_policy(false, true, true),
            AttestationPolicy::ToolMissing
        );
    }

    #[test]
    fn a_release_without_a_bundle_is_refused_rather_than_installed_unchecked() {
        assert_eq!(
            attestation_policy(true, false, false),
            AttestationPolicy::BundleMissing
        );
        assert_eq!(
            attestation_policy(true, false, true),
            AttestationPolicy::BundleMissing
        );
    }
}
