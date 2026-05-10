//! Skill source location + version comparison.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::process::curl_download;
use super::release::REPO;

pub const SKILL_NAME: &str = "symora";

/// Where the SKILL.md to install came from.
#[derive(Debug)]
pub struct SkillSource {
    pub root: PathBuf,
    pub origin: SkillOrigin,
}

#[derive(Debug, Clone)]
pub enum SkillOrigin {
    /// A local checkout (preferred when running from inside the repo).
    LocalCheckout,
    /// Fetched from `raw.githubusercontent.com` for the named git ref.
    Remote { git_ref: String },
}

/// Locate or fetch the skill source. Local checkout wins if present;
/// otherwise we fetch from raw.githubusercontent.com at `git_ref`, falling
/// back to `main` if that ref does not yet have the file.
///
/// `tmp_dir` is used only when we have to fetch — caller controls its
/// lifetime so we never leak temp data into the user's home.
pub fn prepare_skill_source(git_ref: &str, tmp_dir: &Path) -> Result<SkillSource> {
    if let Some(local) = find_local_skill_root() {
        return Ok(SkillSource {
            root: local,
            origin: SkillOrigin::LocalCheckout,
        });
    }

    let dest_root = tmp_dir.join("skill").join(SKILL_NAME);
    std::fs::create_dir_all(&dest_root)
        .with_context(|| format!("creating {}", dest_root.display()))?;
    let dest = dest_root.join("SKILL.md");

    let primary = format!(
        "https://raw.githubusercontent.com/{REPO}/{git_ref}/.claude/skills/{SKILL_NAME}/SKILL.md"
    );
    if curl_download(&primary, &dest).is_ok() {
        return Ok(SkillSource {
            root: dest_root,
            origin: SkillOrigin::Remote {
                git_ref: git_ref.to_string(),
            },
        });
    }

    if git_ref != "main" {
        let fallback = format!(
            "https://raw.githubusercontent.com/{REPO}/main/.claude/skills/{SKILL_NAME}/SKILL.md"
        );
        if curl_download(&fallback, &dest).is_ok() {
            return Ok(SkillSource {
                root: dest_root,
                origin: SkillOrigin::Remote {
                    git_ref: "main".to_string(),
                },
            });
        }
    }

    Err(anyhow!(
        "could not fetch SKILL.md from ref '{git_ref}' or main"
    ))
}

/// Detect a real source checkout — the directory must contain BOTH
/// `Cargo.toml` (proving it is the symora source tree) AND
/// `.claude/skills/symora/SKILL.md`. Without the `Cargo.toml` guard, walking
/// up from `~/.local/bin/symora` would falsely match the user's installed
/// skill at `~/.claude/skills/symora`.
fn find_local_skill_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let from_exe = exe.parent().map(|p| p.to_path_buf());
    let from_cwd = std::env::current_dir().ok();

    for start in [from_exe, from_cwd].into_iter().flatten() {
        if let Some(found) = walk_up_for_checkout(&start) {
            return Some(found);
        }
    }
    None
}

fn walk_up_for_checkout(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    for _ in 0..8 {
        let skill = dir.join(".claude/skills").join(SKILL_NAME);
        if dir.join("Cargo.toml").is_file() && skill.join("SKILL.md").is_file() {
            return Some(skill);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Read the `version:` field from a SKILL.md frontmatter, or `None` if absent.
/// Only inspects the YAML frontmatter (between the opening `---` and the
/// closing `---`); body text is ignored even if it contains `version:`.
pub fn read_skill_version(skill_md: &Path) -> Option<String> {
    let body = std::fs::read_to_string(skill_md).ok()?;
    let mut lines = body.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix("version:") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillVersionDelta {
    Equal,
    IncomingNewer,
    IncomingOlder,
    Unknown,
}

/// Compare an installed version against an incoming one, both as raw
/// `version:` strings (e.g. `0.7.0`, `0.7.0-beta.1`).
pub fn compare_skill_versions(
    installed: Option<&str>,
    incoming: Option<&str>,
) -> SkillVersionDelta {
    match (installed, incoming) {
        (Some(a), Some(b)) if a == b => SkillVersionDelta::Equal,
        (Some(a), Some(b)) => match (parse_semver(a), parse_semver(b)) {
            (Some(ka), Some(kb)) => match ka.cmp(&kb) {
                Ordering::Equal => SkillVersionDelta::Equal,
                Ordering::Less => SkillVersionDelta::IncomingNewer,
                Ordering::Greater => SkillVersionDelta::IncomingOlder,
            },
            _ => SkillVersionDelta::Unknown,
        },
        _ => SkillVersionDelta::Unknown,
    }
}

fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?.trim_start_matches('v');
    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
    let patch: u64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert_eq!(
            compare_skill_versions(Some("0.7.0"), Some("0.7.0")),
            SkillVersionDelta::Equal
        );
        assert_eq!(
            compare_skill_versions(Some("0.6.0"), Some("0.7.0")),
            SkillVersionDelta::IncomingNewer
        );
        assert_eq!(
            compare_skill_versions(Some("0.8.0"), Some("0.7.0")),
            SkillVersionDelta::IncomingOlder
        );
        assert_eq!(
            compare_skill_versions(Some("garbage"), Some("0.7.0")),
            SkillVersionDelta::Unknown
        );
        assert_eq!(
            compare_skill_versions(None, Some("0.7.0")),
            SkillVersionDelta::Unknown
        );
    }

    #[test]
    fn semver_parse() {
        assert_eq!(parse_semver("0.7.0"), Some((0, 7, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_semver("0.7.0-beta.1"), Some((0, 7, 0)));
        assert_eq!(parse_semver("not-a-version"), None);
    }

    #[test]
    fn read_version_from_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: symora\nversion: 0.7.0\n---\n\n# Body\nversion: ignored\n",
        )
        .unwrap();
        assert_eq!(read_skill_version(&path), Some("0.7.0".to_string()));
    }

    #[test]
    fn read_version_ignores_body_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "---\nname: symora\n---\n\nversion: 99.0.0\n").unwrap();
        assert_eq!(read_skill_version(&path), None);
    }

    #[test]
    fn read_version_handles_missing_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "no frontmatter here\nversion: 1.0.0\n").unwrap();
        assert_eq!(read_skill_version(&path), None);
    }

    /// SKILL.md must declare the same version as Cargo.toml. This is the
    /// single mechanism that prevents release-time drift between the binary
    /// and the skill it ships with — a release where they disagree would
    /// make `setup skill` think every install is "newer" or "older" than
    /// the binary, breaking the version-aware update flow.
    #[test]
    fn skill_version_matches_cargo_pkg_version() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let skill_md = manifest_dir.join(".claude/skills/symora/SKILL.md");
        assert!(
            skill_md.is_file(),
            "expected {} to exist; release flow depends on it",
            skill_md.display()
        );
        let declared = read_skill_version(&skill_md).expect(
            "SKILL.md must carry `version: <cargo-version>` in its YAML frontmatter — \
             without it the skill installer cannot detect upgrades",
        );
        assert_eq!(
            declared,
            env!("CARGO_PKG_VERSION"),
            "SKILL.md version drifted from Cargo.toml — bump both together on every release"
        );
    }

    #[test]
    fn local_checkout_requires_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // A user's home with only the installed skill — no Cargo.toml.
        let skill = root.join(".claude/skills").join(SKILL_NAME);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nversion: 0.0.1\n---\n").unwrap();
        assert!(walk_up_for_checkout(root).is_none());

        // Now add a Cargo.toml — looks like a real checkout.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert_eq!(walk_up_for_checkout(root).unwrap(), skill);
    }
}
