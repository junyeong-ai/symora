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

/// The version a release tag names, or `None` for a tag that names none.
fn version_of_tag(tag: &str) -> Option<String> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    (!version.is_empty()).then(|| version.to_string())
}

/// The version the API's answer names.
///
/// Read from the release object's own `tag_name`, which is the first key of
/// that name in the body — the release GitHub calls latest is the whole
/// response, and the fields that follow (author, assets) carry no such key.
pub(super) fn tag_from_api_body(body: &str) -> Option<String> {
    let after = body.split_once("\"tag_name\"")?.1;
    let after = after.split_once(':')?.1;
    let after = after.split_once('"')?.1;
    version_of_tag(after.split_once('"')?.0)
}

/// The version the redirect's destination names.
pub(super) fn tag_from_effective_url(url: &str) -> Option<String> {
    let after = url.split_once("/releases/tag/")?.1;
    version_of_tag(after.split(['/', '?', '#']).next()?)
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
    tag_from_api_body(&body)
}

fn resolve_via_redirect() -> Option<String> {
    tag_from_effective_url(&curl_resolve_redirect(&format!("{RELEASES_URL}/latest")).ok()?)
}

/// The latest release version — asked of the API, and of the web redirect
/// only where the API could not answer.
///
/// Both answer the same question and they disagree for minutes at a time:
/// the redirect on `/releases/tag/` is a view that trails the API after a
/// release is published, which is exactly when someone runs an update. Read
/// in that window it names the release before, and the update built on it
/// calls the running binary current — a wrong answer delivered with the
/// confidence of a right one. So the API settles it, and the redirect
/// answers only when the API cannot: the API's rate limit counts against an
/// unauthenticated IP, which a shared runner can exhaust, and the redirect
/// has no limit to exhaust.
pub fn resolve_latest_version() -> Result<String> {
    if let Some(v) = resolve_via_api() {
        return Ok(v);
    }
    if let Some(v) = resolve_via_redirect() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The API is what settles which release is latest, and it settles it
    /// only while its answer parses. A parse that returns nothing is not a
    /// visible failure — resolution falls through to the redirect, which
    /// trails by minutes after a release and names the one before it. So the
    /// shape the API actually sends is pinned here: pretty-printed, with the
    /// key reached past an `html_url` that carries a release tag of its own.
    #[test]
    fn the_api_answer_is_read_from_the_release_it_describes() {
        let body = r#"{
  "url": "https://api.github.com/repos/junyeong-ai/symora/releases/372327722",
  "html_url": "https://github.com/junyeong-ai/symora/releases/tag/v0.20.0",
  "id": 372327722,
  "author": { "login": "github-actions[bot]", "id": 41898282 },
  "node_id": "RE_kwDO",
  "tag_name": "v0.20.1",
  "target_commitish": "main",
  "name": "v0.20.1",
  "draft": false,
  "prerelease": false
}"#;
        assert_eq!(tag_from_api_body(body).as_deref(), Some("0.20.1"));
        assert_eq!(
            tag_from_api_body(r#"{"tag_name":"v1.0.0-rc.1"}"#).as_deref(),
            Some("1.0.0-rc.1")
        );
        // A tag without the `v` is still a tag; one that is only `v` is not.
        assert_eq!(
            tag_from_api_body(r#"{"tag_name": "0.9.0"}"#).as_deref(),
            Some("0.9.0")
        );
        for empty in [r#"{"tag_name": "v"}"#, r#"{"tag_name": ""}"#, "{}", ""] {
            assert_eq!(tag_from_api_body(empty), None, "body {empty:?}");
        }
    }

    /// The redirect answers when the API cannot, and its destination is a URL
    /// rather than a document — query and fragment are not part of the tag.
    #[test]
    fn the_redirect_answer_is_read_from_where_it_landed() {
        assert_eq!(
            tag_from_effective_url("https://github.com/junyeong-ai/symora/releases/tag/v0.20.1")
                .as_deref(),
            Some("0.20.1")
        );
        assert_eq!(
            tag_from_effective_url(
                "https://github.com/junyeong-ai/symora/releases/tag/v0.20.1?foo=1#bar"
            )
            .as_deref(),
            Some("0.20.1")
        );
        for missed in [
            "https://github.com/junyeong-ai/symora/releases/latest",
            "https://github.com/junyeong-ai/symora/releases/tag/v",
            "https://github.com/junyeong-ai/symora/releases/tag/",
        ] {
            assert_eq!(tag_from_effective_url(missed), None, "url {missed:?}");
        }
    }

    /// Whatever a source answered still has to survive the pattern the
    /// download path builds a URL from — the two are one decision.
    #[test]
    fn every_resolved_version_is_one_the_download_path_accepts() {
        for body in [
            r#"{"tag_name": "v0.20.1"}"#,
            r#"{"tag_name": "v1.0.0-rc.1"}"#,
            r#"{"tag_name": "0.9.0"}"#,
        ] {
            let version = tag_from_api_body(body).expect("body names a tag");
            assert!(is_valid_version(&version), "version {version:?}");
        }
    }
}
