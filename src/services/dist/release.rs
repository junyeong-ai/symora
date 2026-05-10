//! Release artifact resolution and download.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::process::{curl_download, curl_resolve_redirect, have, run_capture};

pub const REPO: &str = "junyeong-ai/symora";
pub const RELEASES_URL: &str = "https://github.com/junyeong-ai/symora/releases";
pub const API_LATEST_URL: &str = "https://api.github.com/repos/junyeong-ai/symora/releases/latest";

/// A downloaded release asset (archive + sidecar checksum), already on disk.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub version: String,
    pub target: String,
    pub archive: PathBuf,
    pub checksum: PathBuf,
}

/// Resolve the latest release version, in two stages:
///   1. follow the redirect on `/releases/latest` (no API rate limit)
///   2. fall back to the GitHub API
fn resolve_via_redirect() -> Option<String> {
    let effective = curl_resolve_redirect(&format!("{RELEASES_URL}/latest")).ok()?;
    let after = effective.split("/releases/tag/").nth(1)?;
    let tag = after.split(['/', '?', '#']).next()?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

fn resolve_via_api() -> Option<String> {
    if !have("curl") {
        return None;
    }
    let body = run_capture(
        "curl",
        &["--fail", "--silent", "--location", API_LATEST_URL],
    )
    .ok()?;
    let needle = "\"tag_name\":";
    let idx = body.find(needle)?;
    let after = &body[idx + needle.len()..];
    let q1 = after.find('"')?;
    let after = &after[q1 + 1..];
    let q2 = after.find('"')?;
    let tag = &after[..q2];
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

pub fn resolve_latest_version() -> Result<String> {
    if let Some(v) = resolve_via_redirect() {
        return Ok(v);
    }
    if let Some(v) = resolve_via_api() {
        return Ok(v);
    }
    Err(anyhow!("could not resolve latest release version"))
}

/// Validate a release version against an injection-resistant pattern.
pub fn is_valid_version(version: &str) -> bool {
    if version.is_empty() || version.len() > 64 {
        return false;
    }
    let mut chars = version.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

/// Download `symora-v<version>-<target>.tar.gz` plus its `.sha256` sidecar
/// into `dest_dir`. Caller is responsible for the temp directory's lifetime.
pub fn download_release(version: &str, target: &str, dest_dir: &Path) -> Result<ReleaseAsset> {
    if !is_valid_version(version) {
        return Err(anyhow!("invalid release version: {version}"));
    }
    let archive_name = format!("symora-v{version}-{target}.tar.gz");
    let archive_url = format!("{RELEASES_URL}/download/v{version}/{archive_name}");
    let archive = dest_dir.join(&archive_name);
    let checksum = dest_dir.join(format!("{archive_name}.sha256"));

    curl_download(&archive_url, &archive).with_context(|| format!("downloading {archive_name}"))?;
    curl_download(&format!("{archive_url}.sha256"), &checksum)
        .with_context(|| format!("downloading {archive_name}.sha256"))?;

    Ok(ReleaseAsset {
        version: version.to_string(),
        target: target.to_string(),
        archive,
        checksum,
    })
}
