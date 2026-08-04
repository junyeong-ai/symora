//! `symora setup skill` — install or update the Claude Code skill.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::cli::utils::ui::{Step, confirm, section, step};
use crate::services::dist::{
    SKILL_NAME, SkillOrigin, SkillVersionDelta, TempDir, compare_skill_versions, display, paths,
    prepare_skill_source, read_skill_version,
};

#[derive(Args, Debug, Default)]
pub struct SkillArgs {
    /// Git ref (tag/branch/sha) to fetch the SKILL.md from when running
    /// outside a checkout. Defaults to the running binary's version.
    #[arg(long, value_name = "REF")]
    pub git_ref: Option<String>,

    /// Always reinstall, even if the version comparison says equal.
    #[arg(long)]
    pub force: bool,
}

#[derive(Serialize, Debug)]
pub struct SkillOutcome {
    pub action: SkillAction,
    /// Version pinned on disk after this run (the kept one for `KeptExisting`,
    /// the incoming one for any of the write actions).
    pub final_version: Option<String>,
    pub incoming_version: Option<String>,
    pub delta: SkillVersionDelta,
    pub destination: String,
    pub origin: SkillOriginRepr,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillAction {
    /// Fresh install — no skill was present beforehand.
    Installed,
    /// Replaced an older version with a newer one.
    Updated,
    /// Replaced an existing version with the same or older one.
    Reinstalled,
    /// Skill was already installed and was left in place.
    KeptExisting,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case", tag = "kind", content = "ref")]
pub enum SkillOriginRepr {
    LocalCheckout,
    Remote(String),
}

pub fn run_skill(args: SkillArgs, assume_yes: bool) -> Result<SkillOutcome> {
    section(format!("setup skill: {SKILL_NAME}"));

    let dest = paths::skill_dir()?;
    let installed_md = dest.join("SKILL.md");
    let installed_version = read_skill_version(&installed_md);

    let git_ref = resolve_ref(args.git_ref.as_deref());
    let tmp = TempDir::new("symora-setup-skill")?;
    let source = prepare_skill_source(&git_ref, tmp.path())?;
    let incoming_md = source.root.join("SKILL.md");
    let incoming_version = read_skill_version(&incoming_md);

    let (origin_label, origin_repr) = origin_view(&source.origin);
    let delta = compare_skill_versions(installed_version.as_deref(), incoming_version.as_deref());

    step(
        Step::Info,
        format!(
            "incoming v{} ({})",
            incoming_version.as_deref().unwrap_or("unknown"),
            origin_label,
        ),
    );

    let already_installed = installed_md.is_file();

    if !already_installed {
        install_fresh(&source.root, &dest)?;
        return Ok(SkillOutcome {
            action: SkillAction::Installed,
            final_version: incoming_version.clone(),
            incoming_version,
            delta,
            destination: display(&dest),
            origin: origin_repr,
        });
    }

    step(
        Step::Info,
        format!(
            "installed v{}",
            installed_version.as_deref().unwrap_or("unknown"),
        ),
    );

    let action = if args.force {
        SkillAction::Reinstalled
    } else {
        decide_action(delta, assume_yes)
    };

    let final_version = match action {
        SkillAction::KeptExisting => {
            step(Step::Skip, "kept existing skill");
            installed_version.clone()
        }
        SkillAction::Installed | SkillAction::Updated | SkillAction::Reinstalled => {
            replace_dir(&source.root, &dest)?;
            step(Step::Ok, format!("skill installed at {}", display(&dest)));
            incoming_version.clone()
        }
    };

    Ok(SkillOutcome {
        action,
        final_version,
        incoming_version,
        delta,
        destination: display(&dest),
        origin: origin_repr,
    })
}

fn origin_view(origin: &SkillOrigin) -> (String, SkillOriginRepr) {
    match origin {
        SkillOrigin::LocalCheckout => {
            ("local checkout".to_string(), SkillOriginRepr::LocalCheckout)
        }
        SkillOrigin::Remote { git_ref } => (
            format!("ref {git_ref}"),
            SkillOriginRepr::Remote(git_ref.clone()),
        ),
    }
}

fn decide_action(delta: SkillVersionDelta, assume_yes: bool) -> SkillAction {
    match delta {
        SkillVersionDelta::Equal => {
            if confirm("Reinstall the same version?", false, assume_yes) {
                SkillAction::Reinstalled
            } else {
                SkillAction::KeptExisting
            }
        }
        SkillVersionDelta::IncomingNewer => {
            if confirm("Update to incoming version?", true, assume_yes) {
                SkillAction::Updated
            } else {
                SkillAction::KeptExisting
            }
        }
        SkillVersionDelta::IncomingOlder => {
            if confirm("Downgrade to incoming version?", false, assume_yes) {
                SkillAction::Updated
            } else {
                SkillAction::KeptExisting
            }
        }
        SkillVersionDelta::Unknown => {
            if confirm(
                "Reinstall (versions could not be compared)?",
                false,
                assume_yes,
            ) {
                SkillAction::Reinstalled
            } else {
                SkillAction::KeptExisting
            }
        }
    }
}

fn install_fresh(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    replace_dir(source, dest)?;
    step(Step::Ok, format!("skill installed at {}", display(dest)));
    Ok(())
}

fn replace_dir(source: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).with_context(|| format!("removing {}", dest.display()))?;
    }
    copy_recursively(source, dest).with_context(|| format!("copying skill into {}", dest.display()))
}

fn copy_recursively(src: &Path, dst: &Path) -> Result<()> {
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        copy_recursively(&entry.path(), &target)?;
    }
    Ok(())
}

/// Default the git ref to the running binary's release tag. `CARGO_PKG_VERSION`
/// is guaranteed non-empty by Cargo, so no further fallback is needed.
fn resolve_ref(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")))
}
