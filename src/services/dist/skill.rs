//! Skill source location + version comparison.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
    Bundled,
    Remote { git_ref: String },
}

pub fn prepare_skill_source(git_ref: Option<&str>, tmp_dir: &Path) -> Result<SkillSource> {
    let root = tmp_dir.join("skill").join(SKILL_NAME);
    std::fs::create_dir_all(&root)?;
    let dest = root.join("SKILL.md");
    let origin = if let Some(git_ref) = git_ref {
        let url = format!(
            "https://raw.githubusercontent.com/{REPO}/{git_ref}/.claude/skills/{SKILL_NAME}/SKILL.md"
        );
        curl_download(&url, &dest)
            .with_context(|| format!("could not fetch SKILL.md from ref '{git_ref}'"))?;
        SkillOrigin::Remote {
            git_ref: git_ref.to_string(),
        }
    } else {
        std::fs::write(
            &dest,
            include_str!("../../../.claude/skills/symora/SKILL.md"),
        )?;
        SkillOrigin::Bundled
    };
    Ok(SkillSource { root, origin })
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
            let v = rest.trim().trim_matches(['"', '\'']);
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

fn parse_semver(version: &str) -> Option<semver::Version> {
    let version = version.trim_start_matches('v');
    let components = version.split(['-', '+']).next()?.split('.').count();
    let version = match components {
        1 => format!("{version}.0.0"),
        2 => format!("{version}.0"),
        _ => version.to_string(),
    };
    semver::Version::parse(&version).ok()
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
        assert_eq!(parse_semver("0.7.0"), Some(semver::Version::new(0, 7, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some(semver::Version::new(1, 2, 3)));
        assert_eq!(parse_semver("1.2"), Some(semver::Version::new(1, 2, 0)));
        assert_eq!(
            compare_skill_versions(Some("0.7.0-beta.1"), Some("0.7.0")),
            SkillVersionDelta::IncomingNewer
        );
        assert_eq!(parse_semver("not-a-version"), None);
    }

    #[test]
    fn read_version_from_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: symora\nmetadata:\n  version: 0.7.0\n---\n\n# Body\nversion: ignored\n",
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
            "SKILL.md must carry `metadata.version: <cargo-version>` in its YAML frontmatter — \
             without it the skill installer cannot detect upgrades",
        );
        assert_eq!(
            declared,
            env!("CARGO_PKG_VERSION"),
            "SKILL.md version drifted from Cargo.toml — bump both together on every release"
        );
    }

    #[test]
    fn bundled_skill_matches_the_running_binary() {
        let dir = tempfile::tempdir().unwrap();
        let source = prepare_skill_source(None, dir.path()).unwrap();
        assert!(matches!(source.origin, SkillOrigin::Bundled));
        assert_eq!(
            read_skill_version(&source.root.join("SKILL.md")).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }
}
