use std::path::PathBuf;
use std::process::Command;

pub const SYMORA: &str = env!("CARGO_BIN_EXE_symora");

/// A `symora` command that reads its global configuration from a directory
/// holding none, so a setting in the developer's own `~/.config/symora`
/// cannot change what a test observes. A test that needs a global
/// configuration sets its own `XDG_CONFIG_HOME` over this one.
pub fn symora() -> Command {
    let mut command = Command::new(SYMORA);
    command.env("XDG_CONFIG_HOME", no_global_config());
    command
}

pub fn no_global_config() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-global-config")
}
