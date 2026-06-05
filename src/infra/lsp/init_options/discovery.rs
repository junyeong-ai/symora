//! Root-aware toolchain discovery for init options.
//!
//! One reusable rule: probe a fixed candidate list, accept the first
//! entry that is a real executable on disk, otherwise decline — the
//! language server then falls back to its own auto-discovery. Declining
//! to guess is the opposite of guessing: handing a server a path that
//! merely *looks* right (an empty `.venv/`, a venv from another project)
//! turns every import into a phantom diagnostic.

use std::path::{Path, PathBuf};

/// Interpreter inside a virtualenv, platform-correct layout.
fn venv_interpreter(venv_root: &Path) -> PathBuf {
    if cfg!(windows) {
        venv_root.join("Scripts").join("python.exe")
    } else {
        venv_root.join("bin").join("python")
    }
}

/// "Demonstrably exists" means an executable file, not merely a file —
/// a venv whose interpreter lost its mode bits is as broken as no venv.
fn is_executable_interpreter(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The project's venv Python interpreter, when one demonstrably exists.
///
/// Candidate order, each validated as an existing interpreter binary
/// (never a bare directory check):
///
/// 1. `$VIRTUAL_ENV` — accepted only when it canonicalizes *inside* the
///    project root. The daemon inherits its environment once at boot and
///    serves many projects; an unscoped `VIRTUAL_ENV` would leak one
///    project's interpreter into every other project's server.
/// 2. `<root>/.venv`
/// 3. `<root>/venv`
///
/// Subpackage venvs are deliberately not searched: picking one of many
/// is picking arbitrarily.
pub fn venv_python(root: &Path) -> Option<PathBuf> {
    let canonical_root = root.canonicalize().ok()?;

    if let Some(env_venv) = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from)
        && let Ok(canonical_venv) = env_venv.canonicalize()
        && canonical_venv.starts_with(&canonical_root)
    {
        let interpreter = venv_interpreter(&canonical_venv);
        if is_executable_interpreter(&interpreter) {
            return Some(interpreter);
        }
    }

    [".venv", "venv"]
        .into_iter()
        .map(|name| venv_interpreter(&canonical_root.join(name)))
        .find(|interpreter| is_executable_interpreter(interpreter))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_venv(root: &Path, name: &str) -> PathBuf {
        let venv = root.join(name);
        let bin = venv.join(if cfg!(windows) { "Scripts" } else { "bin" });
        std::fs::create_dir_all(&bin).unwrap();
        let interpreter = bin.join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        std::fs::write(&interpreter, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        interpreter
    }

    #[test]
    fn finds_dot_venv_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let interpreter = make_venv(dir.path(), ".venv");
        assert_eq!(
            venv_python(dir.path()),
            Some(interpreter.canonicalize().unwrap_or(interpreter))
        );
    }

    #[test]
    fn dot_venv_wins_over_venv() {
        let dir = tempfile::tempdir().unwrap();
        let dot = make_venv(dir.path(), ".venv");
        make_venv(dir.path(), "venv");
        assert_eq!(
            venv_python(dir.path()),
            Some(dot.canonicalize().unwrap_or(dot))
        );
    }

    #[test]
    fn empty_venv_dir_is_declined() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".venv")).unwrap();
        assert_eq!(venv_python(dir.path()), None);
    }

    #[test]
    fn venv_as_plain_file_is_declined() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".venv"), "not a venv").unwrap();
        assert_eq!(venv_python(dir.path()), None);
    }

    #[test]
    fn missing_root_yields_none() {
        assert_eq!(venv_python(Path::new("/definitely/not/a/root")), None);
    }
}
