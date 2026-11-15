//! Single source of truth for the user-level paths the lifecycle commands
//! touch. Mirrors the conventions used elsewhere in the crate:
//!
//! - `config_dir` is where the config service reads the global configuration
//!   (XDG_CONFIG_HOME with `~/.config` fallback).
//! - `daemon_dir` matches `daemon::server::config::DaemonRuntimeConfig::load`
//!   (`$HOME/.symora`, falling back to `/tmp/.symora` when HOME is unset).
//! - `skill_dir` is owned here — Claude Code's `~/.claude/skills/<name>`
//!   convention is not used by anything else in the crate.
//!
//! Anything outside this module that constructs these paths by hand is a
//! bug — drift will silently desync `setup` and `self` from where the
//! daemon and config service actually read/write.
//!
//! Every path derives from one [`UserEnv`], which a run reads from its
//! environment and a unit test never does: see [`UserEnv::current`].

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use super::skill::SKILL_NAME;

/// What the user-level paths derive from.
struct UserEnv {
    home: Option<PathBuf>,
    config_home: Option<PathBuf>,
}

impl UserEnv {
    #[cfg(not(test))]
    fn current() -> Self {
        Self {
            home: dirs::home_dir(),
            config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        }
    }

    /// A unit test runs in a process whose environment is the developer's,
    /// shared with every other test. Read from it, a test would answer under
    /// their global configuration and leave daemon files in their home, and
    /// one test changing it races every other. A test thread sees instead
    /// the home a [`test_home::TestHome`] gives it, or one nothing can
    /// create.
    #[cfg(test)]
    fn current() -> Self {
        Self {
            home: Some(test_home::current()),
            config_home: None,
        }
    }

    /// `$HOME` if available, `/tmp` otherwise — same fallback the daemon
    /// uses so a HOME-less environment still has a deterministic place to land.
    fn home_or_tmp(&self) -> PathBuf {
        self.home.clone().unwrap_or_else(|| PathBuf::from("/tmp"))
    }

    /// `${XDG_CONFIG_HOME:-$HOME/.config}/symora`.
    fn config_dir(&self) -> PathBuf {
        self.config_home
            .clone()
            .or_else(|| self.home.as_ref().map(|h| h.join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("symora")
    }

    /// `p` with `$HOME/` collapsed.
    fn display(&self, p: &Path) -> String {
        if let Some(home) = &self.home {
            if p == home {
                return "$HOME".to_string();
            }
            if let Ok(rest) = p.strip_prefix(home) {
                return format!("$HOME/{}", rest.display());
            }
        }
        p.display().to_string()
    }
}

/// `$HOME` if available, `/tmp` otherwise.
pub fn home_or_tmp() -> PathBuf {
    UserEnv::current().home_or_tmp()
}

/// `$HOME` or an explicit error. Use this when an unresolvable HOME should
/// fail loudly (e.g. installing the skill).
pub fn home() -> Result<PathBuf> {
    UserEnv::current()
        .home
        .ok_or_else(|| anyhow!("could not determine home directory"))
}

/// `$HOME/.claude/skills/symora` — where the Claude Code skill lives.
pub fn skill_dir() -> Result<PathBuf> {
    Ok(home()?.join(".claude").join("skills").join(SKILL_NAME))
}

/// `${XDG_CONFIG_HOME:-$HOME/.config}/symora`.
pub fn config_dir() -> PathBuf {
    UserEnv::current().config_dir()
}

/// `$HOME/.symora` (or `/tmp/.symora`) — daemon runtime data.
pub fn daemon_dir() -> PathBuf {
    home_or_tmp().join(".symora")
}

/// Render a path with `$HOME/` collapsed for human-friendly output.
pub fn display(p: &Path) -> String {
    UserEnv::current().display(p)
}

/// The home a unit test runs under.
#[cfg(test)]
pub(crate) mod test_home {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    /// The home of a test that names none. `/dev/null` is no directory, so
    /// nothing is configured under this path and nothing can be created
    /// there, by root either.
    const UNREACHABLE: &str = "/dev/null/symora-test-home";

    thread_local! {
        static HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub(super) fn current() -> PathBuf {
        HOME.with(|home| home.borrow().clone())
            .unwrap_or_else(|| PathBuf::from(UNREACHABLE))
    }

    /// Gives the calling thread `dir` as its home until dropped, for a test
    /// that writes where a run would write in the user's home. A
    /// `#[tokio::test]` runs on the thread that created it, so the home holds
    /// for everything the test awaits.
    #[must_use]
    pub(crate) struct TestHome(Option<PathBuf>);

    impl TestHome {
        pub(crate) fn new(dir: &Path) -> Self {
            Self(HOME.with(|home| home.replace(Some(dir.to_path_buf()))))
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            HOME.with(|home| *home.borrow_mut() = self.0.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: Option<&str>, config_home: Option<&str>) -> UserEnv {
        UserEnv {
            home: home.map(PathBuf::from),
            config_home: config_home.map(PathBuf::from),
        }
    }

    #[test]
    fn config_dir_respects_xdg_config_home() {
        assert_eq!(
            env(Some("/home/u"), Some("/xdg")).config_dir(),
            PathBuf::from("/xdg/symora")
        );
        assert_eq!(
            env(Some("/home/u"), None).config_dir(),
            PathBuf::from("/home/u/.config/symora")
        );
    }

    #[test]
    fn display_collapses_home() {
        let home = env(Some("/home/u"), None);
        assert_eq!(
            home.display(Path::new("/home/u/.claude/skills/symora")),
            "$HOME/.claude/skills/symora"
        );
        assert_eq!(home.display(Path::new("/home/u")), "$HOME");
        assert_eq!(home.display(Path::new("/etc/passwd")), "/etc/passwd");
    }

    /// A test sees none of the developer's home: nothing is configured where
    /// it looks, and nothing can be created there.
    #[test]
    fn a_test_without_a_home_reaches_nothing() {
        let config = config_dir().join("config.toml");
        assert!(!config.exists(), "{}", config.display());
        assert!(std::fs::create_dir_all(daemon_dir()).is_err());
    }

    /// A [`test_home::TestHome`] holds on its thread until dropped, and
    /// restores the home it replaced.
    #[test]
    fn a_test_home_is_scoped_to_its_guard() {
        let outer = tempfile::tempdir().unwrap();
        let inner = tempfile::tempdir().unwrap();
        let _outer = test_home::TestHome::new(outer.path());
        {
            let _inner = test_home::TestHome::new(inner.path());
            assert_eq!(daemon_dir(), inner.path().join(".symora"));
        }
        assert_eq!(daemon_dir(), outer.path().join(".symora"));
        assert_eq!(config_dir(), outer.path().join(".config/symora"));
        let outer = outer.path().to_path_buf();
        std::thread::spawn(move || assert!(!daemon_dir().starts_with(&outer)))
            .join()
            .unwrap();
    }
}
