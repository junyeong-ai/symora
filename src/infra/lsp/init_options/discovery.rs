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

/// The directories the venv's `.pth` files put on the import path that hold
/// the project's own code: inside the project root and outside the venv, as
/// an editable install of one of its packages names them. Read as Python's
/// `site` reads them, a line naming a directory relative to the
/// site-packages directory; a comment, an `import` line or a missing path
/// names none.
pub fn editable_source_roots(root: &Path, interpreter: &Path) -> Vec<PathBuf> {
    let (Ok(root), Some(Ok(venv))) = (
        root.canonicalize(),
        interpreter
            .parent()
            .and_then(Path::parent)
            .map(Path::canonicalize),
    ) else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for site in site_packages(&venv) {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&site)
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| {
                path.extension().is_some_and(|extension| extension == "pth")
                    && !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            })
            .collect();
        files.sort();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for line in text.trim_start_matches('\u{feff}').lines() {
                if let Ok(dir) = site.join(line.trim_end()).canonicalize()
                    && dir.starts_with(&root)
                    && !dir.starts_with(&venv)
                    && !roots.contains(&dir)
                {
                    roots.push(dir);
                }
            }
        }
    }
    roots
}

/// `$VIRTUAL_ENV`'s interpreter wherever it lies, when one demonstrably
/// exists there. [`venv_python`] hands it to a server only inside the
/// project root; outside it, it is what a server left to choose for itself
/// may take, so its installs are evidence about that choice, never proof.
pub fn virtual_env_python() -> Option<PathBuf> {
    let venv = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from)?;
    let interpreter = venv_interpreter(&venv);
    is_executable_interpreter(&interpreter).then_some(interpreter)
}

/// A distribution an environment records as installed in editable mode from
/// a local directory: PEP 610's `direct_url.json` with `dir_info.editable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableInstall {
    /// Its name as its `METADATA` declares it, PEP 503-normalized.
    pub name: String,
    /// The directory it was installed from, as `direct_url.json`'s `url`
    /// names it: canonical where it exists, as written where it does not.
    pub source: PathBuf,
}

/// A distribution name as PEP 503 compares names: lowercase, each run of
/// `-`, `_` and `.` one `-`.
pub fn normalized_distribution_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for c in name.trim().chars() {
        if matches!(c, '-' | '_' | '.') {
            if !normalized.ends_with('-') {
                normalized.push('-');
            }
        } else {
            normalized.extend(c.to_lowercase());
        }
    }
    normalized
}

/// The distributions the environment of `interpreter` installs editable whose
/// normalized name is one of `names`, read from its `.dist-info` directories
/// as installers record them (PEP 376, 566, 610).
///
/// A `.dist-info` directory is named `{name}-{version}`, so one whose
/// normalized stem does not begin with a sought name and a `-` records
/// another distribution; one that does is confirmed by its `METADATA`
/// `Name`, since `aix-core-extra` begins with `aix-core-` too. Limits: the
/// environment is the interpreter's prefix, `<prefix>/lib/python*/site-packages`
/// (`Lib/site-packages` on Windows), so an interpreter whose packages live
/// elsewhere (a distribution's `dist-packages`, the user site) reads as
/// installing nothing; an install recorded without PEP 610 (a legacy
/// `.egg-link`, `setup.py develop`) names no source and is not read; and an
/// install that is no editable one, a copy of the files in site-packages,
/// is not read either.
pub fn editable_installs(interpreter: &Path, names: &[String]) -> Vec<EditableInstall> {
    let Some(Ok(prefix)) = interpreter
        .parent()
        .and_then(Path::parent)
        .map(Path::canonicalize)
    else {
        return Vec::new();
    };
    let mut installs = Vec::new();
    for site in site_packages(&prefix) {
        let mut records: Vec<PathBuf> = std::fs::read_dir(&site)
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix(".dist-info"))
                    .is_some_and(|stem| {
                        let stem = normalized_distribution_name(stem);
                        names.iter().any(|name| {
                            stem.strip_prefix(name.as_str())
                                .is_some_and(|rest| rest.starts_with('-'))
                        })
                    })
            })
            .collect();
        records.sort();
        for record in records {
            let Some(name) = metadata_name(&record).map(|name| normalized_distribution_name(&name))
            else {
                continue;
            };
            if !names.contains(&name) || installs.iter().any(|i: &EditableInstall| i.name == name) {
                continue;
            }
            if let Some(source) = editable_source(&record) {
                installs.push(EditableInstall { name, source });
            }
        }
    }
    installs
}

/// The `Name` header of a `.dist-info` directory's `METADATA`: the headers
/// end at the first empty line, and a header's name is read without case.
fn metadata_name(record: &Path) -> Option<String> {
    let text = std::fs::read_to_string(record.join("METADATA")).ok()?;
    text.lines()
        .take_while(|line| !line.trim().is_empty())
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("name")
                .then(|| value.trim().to_string())
        })
}

/// The local directory a `.dist-info` directory's `direct_url.json` records
/// an editable install from (PEP 610: a `file://` `url` with
/// `dir_info.editable` true); `None` for any other install.
fn editable_source(record: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(record.join("direct_url.json")).ok()?;
    let direct: serde_json::Value = serde_json::from_str(&text).ok()?;
    if direct["dir_info"]["editable"].as_bool() != Some(true) {
        return None;
    }
    let url = direct["url"].as_str()?;
    // A `file:` URL names a local path, with an empty or `localhost` host.
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let source = crate::models::lsp::uri_to_path(&format!("file://{rest}"));
    Some(source.canonicalize().unwrap_or(source))
}

/// The venv's site-packages directories: `lib/<python>/site-packages`, and
/// `Lib/site-packages` on Windows.
fn site_packages(venv: &Path) -> Vec<PathBuf> {
    if cfg!(windows) {
        return vec![venv.join("Lib").join("site-packages")];
    }
    let mut directories: Vec<PathBuf> = std::fs::read_dir(venv.join("lib"))
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path().join("site-packages")))
        .filter(|directory| directory.is_dir())
        .collect();
    directories.sort();
    directories
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

    /// Each `.pth` path inside the project and outside the venv, in the order
    /// Python adds them; not a comment, an `import` line, a hidden file, a
    /// missing path, another project's or the venv's own.
    #[test]
    fn editable_source_roots_are_the_projects_own_pth_paths() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let interpreter = make_venv(dir.path(), ".venv");
        let site = dir.path().join(".venv/lib/python3.12/site-packages");
        for created in ["packages/a/src", "packages/b", "packages/hidden"] {
            std::fs::create_dir_all(dir.path().join(created)).unwrap();
        }
        std::fs::create_dir_all(site.join("win32")).unwrap();
        let path = |relative: &str| dir.path().join(relative).display().to_string();
        std::fs::write(
            site.join("_a.pth"),
            format!(
                "# comment\nimport sys\n{}\n\n{}\n",
                path("packages/a/src"),
                elsewhere.path().display()
            ),
        )
        .unwrap();
        std::fs::write(
            site.join("b.pth"),
            format!("../../../../packages/b\nwin32\n{}\n", path("missing")),
        )
        .unwrap();
        std::fs::write(site.join(".hidden.pth"), path("packages/hidden")).unwrap();

        let canonical = |relative: &str| dir.path().join(relative).canonicalize().unwrap();
        assert_eq!(
            editable_source_roots(dir.path(), &interpreter),
            vec![canonical("packages/a/src"), canonical("packages/b")]
        );
    }

    /// A `.dist-info` directory recording `name`, with `direct_url` as its
    /// `direct_url.json` where one is given.
    fn record(site: &Path, stem: &str, name: &str, direct_url: Option<&str>) {
        let record = site.join(format!("{stem}.dist-info"));
        std::fs::create_dir_all(&record).unwrap();
        std::fs::write(
            record.join("METADATA"),
            format!("Metadata-Version: 2.4\nname: {name}\nVersion: 0.1.0\n\nName: not-a-header\n"),
        )
        .unwrap();
        if let Some(direct_url) = direct_url {
            std::fs::write(record.join("direct_url.json"), direct_url).unwrap();
        }
    }

    /// Each sought distribution the environment records as installed
    /// editable, by its METADATA name and from the directory its
    /// `direct_url.json` names, percent-decoded: not one installed by copy,
    /// one from an archive, one whose stem merely begins with a sought name,
    /// or one not sought.
    #[cfg(unix)]
    #[test]
    fn editable_installs_are_the_pep_610_editable_records_of_sought_names() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let source = elsewhere.path().join("core lib");
        std::fs::create_dir_all(&source).unwrap();
        let interpreter = make_venv(dir.path(), ".venv");
        let site = dir.path().join(".venv/lib/python3.12/site-packages");
        let editable =
            |path: &str| format!(r#"{{"url": "file://{path}", "dir_info": {{"editable": true}}}}"#);
        let encoded = format!("{}/core%20lib", elsewhere.path().display());
        record(
            &site,
            "Core_Lib-0.1.0",
            "Core.Lib",
            Some(&editable(&encoded)),
        );
        record(
            &site,
            "core_lib_extra-1.0",
            "core-lib-extra",
            Some(&editable("/x")),
        );
        record(
            &site,
            "copied-1.0",
            "copied",
            Some(r#"{"url": "file:///y", "dir_info": {}}"#),
        );
        record(
            &site,
            "archived-1.0",
            "archived",
            Some(r#"{"url": "https://h/a.whl", "archive_info": {}}"#),
        );
        record(&site, "unsought-1.0", "unsought", Some(&editable("/z")));

        let names: Vec<String> = ["core-lib", "copied", "archived"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            editable_installs(&interpreter, &names),
            vec![EditableInstall {
                name: "core-lib".to_string(),
                source: source.canonicalize().unwrap(),
            }]
        );
    }

    #[test]
    fn distribution_names_normalize_as_pep_503_compares_them() {
        for (written, normalized) in [
            ("aix-core", "aix-core"),
            ("Aix_Core", "aix-core"),
            ("aix.__core", "aix-core"),
            ("  PyYAML ", "pyyaml"),
        ] {
            assert_eq!(
                normalized_distribution_name(written),
                normalized,
                "{written}"
            );
        }
    }

    #[test]
    fn missing_root_yields_none() {
        assert_eq!(venv_python(Path::new("/definitely/not/a/root")), None);
    }
}
