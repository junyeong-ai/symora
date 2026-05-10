//! Single source of truth for the user-level paths the lifecycle commands
//! touch. Mirrors the conventions used elsewhere in the crate:
//!
//! - `config_dir` follows `services::config::DefaultConfigService::global_config_path`
//!   (XDG_CONFIG_HOME with `~/.config` fallback).
//! - `daemon_dir` matches `daemon::server::config::DaemonRuntimeConfig::load`
//!   (`$HOME/.symora`, falling back to `/tmp/.symora` when HOME is unset).
//! - `skill_dir` is owned here — Claude Code's `~/.claude/skills/<name>`
//!   convention is not used by anything else in the crate.
//!
//! Anything outside this module that constructs these paths by hand is a
//! bug — drift will silently desync `setup` and `self` from where the
//! daemon and config service actually read/write.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use super::skill::SKILL_NAME;

/// `$HOME` if available, `/tmp` otherwise — same fallback the daemon uses
/// so a HOME-less environment still has a deterministic place to land.
pub fn home_or_tmp() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `$HOME` or an explicit error. Use this when an unresolvable HOME should
/// fail loudly (e.g. installing the skill).
pub fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))
}

/// `$HOME/.claude/skills/symora` — where the Claude Code skill lives.
pub fn skill_dir() -> Result<PathBuf> {
    Ok(home()?.join(".claude").join("skills").join(SKILL_NAME))
}

/// `${XDG_CONFIG_HOME:-$HOME/.config}/symora` — same lookup as
/// `services::config::DefaultConfigService::global_config_path`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("symora")
}

/// `$HOME/.symora` (or `/tmp/.symora`) — daemon runtime data.
pub fn daemon_dir() -> PathBuf {
    home_or_tmp().join(".symora")
}

/// Render a path with `$HOME/` collapsed for human-friendly output.
pub fn display(p: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if p == home {
            return "$HOME".to_string();
        }
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("$HOME/{}", rest.display());
        }
    }
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_respects_xdg_config_home() {
        let original = std::env::var_os("XDG_CONFIG_HOME");

        // Safety: env mutation in a test process. We restore below; tests in
        // this module never read XDG concurrently.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-fake") };
        let got = config_dir();
        assert_eq!(got, PathBuf::from("/tmp/xdg-fake/symora"));

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        let fallback = config_dir();
        assert!(
            fallback.ends_with(".config/symora"),
            "fallback should be ~/.config/symora, got {fallback:?}"
        );

        // Restore.
        if let Some(v) = original {
            unsafe { std::env::set_var("XDG_CONFIG_HOME", v) };
        }
    }

    #[test]
    fn display_collapses_home() {
        if let Some(home) = dirs::home_dir() {
            let inside = home.join(".claude/skills/symora");
            assert_eq!(display(&inside), "$HOME/.claude/skills/symora");
            assert_eq!(display(&home), "$HOME");
        }
        assert_eq!(display(Path::new("/etc/passwd")), "/etc/passwd");
    }
}
