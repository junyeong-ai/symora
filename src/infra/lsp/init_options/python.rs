use std::path::Path;

use serde_json::{Value, json};

use super::discovery::{editable_source_roots, venv_python};
use super::exclude::lsp_exclude_globs;

pub(super) fn python_init_options(root: &Path) -> Value {
    let mut options = json!({
        "python": {
            "analysis": {
                "autoSearchPaths": true,
                "useLibraryCodeForTypes": true,
                "diagnosticMode": "openFilesOnly",
                "autoImportCompletions": false,
                "indexing": true,
                "logLevel": "Information",
                // Derived from the native-index ignore policy so pyright's
                // workspace scan and the index agree on which files exist —
                // see init_options/exclude.rs. A hand-kept literal here drifts
                // (it once missed the host's `.<tool>/worktrees/<slug>` trees,
                // double-counting every ref into an in-flight worktree copy).
                "exclude": lsp_exclude_globs(root)

            }
        }
    });

    // Point pyright at the project venv's interpreter when one exists.
    // `pythonPath` is the single setting pyright honors over LSP for
    // this (it runs the interpreter to derive site-packages itself);
    // `venv`/`venvPath` are pyrightconfig.json-only and ignored here.
    // Without a venv pyright falls back to its own auto-discovery —
    // declining to guess beats injecting a wrong interpreter.
    if let Some(interpreter) = venv_python(root) {
        options["python"]["pythonPath"] = json!(interpreter.to_string_lossy());
        // pyright reads a module it first reaches through site-packages as a
        // third-party import and drops its errors and warnings, even once it
        // is opened, and the project's own packages installed editable reach
        // each other that way. On the extra paths they are local.
        let roots = editable_source_roots(root, &interpreter);
        if !roots.is_empty() {
            options["python"]["analysis"]["extraPaths"] = json!(
                roots
                    .iter()
                    .map(|root| root.to_string_lossy())
                    .collect::<Vec<_>>()
            );
        }
    }

    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_python_path_when_venv_exists() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir
            .path()
            .join(".venv")
            .join(if cfg!(windows) { "Scripts" } else { "bin" });
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

        let options = python_init_options(dir.path());
        let injected = options["python"]["pythonPath"].as_str().unwrap();
        assert!(injected.ends_with(if cfg!(windows) {
            "python.exe"
        } else {
            "bin/python"
        }));
    }

    #[test]
    fn omits_python_path_without_venv() {
        let dir = tempfile::tempdir().unwrap();
        let options = python_init_options(dir.path());
        assert!(options["python"].get("pythonPath").is_none());
    }

    #[test]
    fn exclude_is_derived_from_ignore_policy() {
        // The exclude is the shared policy, not a hand-kept literal — so it
        // carries the hidden-directory class (the worktree fix) and stays in
        // lockstep with the native index.
        let dir = tempfile::tempdir().unwrap();
        let options = python_init_options(dir.path());
        let exclude = options["python"]["analysis"]["exclude"].as_array().unwrap();
        let derived = lsp_exclude_globs(dir.path());
        assert_eq!(exclude.len(), derived.len());
        assert!(exclude.iter().any(|v| v == "**/.*"));
    }
}
