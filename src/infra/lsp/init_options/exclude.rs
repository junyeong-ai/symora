//! LSP `exclude`-style globs derived from the project's ignore policy.
//!
//! A language server's workspace scan and Symora's native index must agree on
//! which files exist (CLAUDE.md invariant 3). When they disagree, `refs` /
//! `impact` count call sites in directories the index never walked — a git
//! worktree a host project parks under a dotted dir, a vendored copy — and
//! inflate the very coverage numbers an agent trusts. So the `exclude` handed
//! to a server is computed from the same [`FileFilter`] policy the index uses,
//! never a hand-maintained per-language literal that drifts from it.

use std::collections::BTreeSet;
use std::path::Path;

use crate::infra::file_filter::{DEFAULT_IGNORE_PATTERNS, FileFilter};

/// Exclude globs reflecting the native-index ignore policy: the hidden-directory
/// class (mirrors `FileFilter`'s `include_hidden = false`), the shared default
/// ignore directories, and the project's own top-level `.gitignore` /
/// `.symora/ignore` entries. Names — not absolute paths — so a nested copy is
/// excluded too; returned sorted for a stable `initializationOptions` payload.
pub(super) fn lsp_exclude_globs(root: &Path) -> Vec<String> {
    let mut globs: BTreeSet<String> = BTreeSet::new();

    // Every hidden directory in one glob — keeps a server out of `.git`,
    // `.venv`, and any `.<tool>/…` working tree (the dotted dir a host project
    // parks git worktrees under). Matches the index's `include_hidden = false`
    // and pyright's own default.
    globs.insert("**/.*".to_string());

    // Shared default ignore directories. Hidden names are already covered above;
    // file / glob entries (`*.log`, `gradle-wrapper.jar`) carry no source a
    // server indexes, so only plain directory names contribute.
    for &pattern in DEFAULT_IGNORE_PATTERNS {
        if !pattern.contains('.') && !pattern.contains('*') {
            globs.insert(format!("**/{pattern}"));
        }
    }

    // Project-specific top-level directories the ignore policy excludes
    // (`.gitignore` / `.symora/ignore`) that the defaults do not already name.
    let filter = FileFilter::with_gitignore(root);
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && filter.is_ignored(&path)
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && !name.starts_with('.')
            {
                globs.insert(format!("**/{name}"));
            }
        }
    }

    globs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn includes_hidden_directory_class() {
        // The single glob that keeps a server out of every dotted working tree
        // (`.git`, `.venv`, the `.<tool>/worktrees/<slug>` a host parks specs in).
        let globs = lsp_exclude_globs(TempDir::new().unwrap().path());
        assert!(globs.contains(&"**/.*".to_string()));
    }

    #[test]
    fn includes_shared_default_directories() {
        let globs = lsp_exclude_globs(TempDir::new().unwrap().path());
        assert!(globs.contains(&"**/node_modules".to_string()));
        assert!(globs.contains(&"**/target".to_string()));
        assert!(globs.contains(&"**/vendor".to_string()));
    }

    #[test]
    fn omits_file_and_glob_default_patterns() {
        // `*.log` / `gradle-wrapper.jar` are not directories a server scans.
        let globs = lsp_exclude_globs(TempDir::new().unwrap().path());
        assert!(!globs.iter().any(|g| g.contains("*.log")));
        assert!(!globs.iter().any(|g| g.contains("gradle-wrapper")));
        // Hidden defaults are folded into the hidden class, never named twice.
        assert!(!globs.contains(&"**/.venv".to_string()));
    }

    #[test]
    fn honors_project_gitignore_top_level() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "buildcache/\n").unwrap();
        fs::create_dir(root.join("buildcache")).unwrap();

        let globs = lsp_exclude_globs(root);
        assert!(globs.contains(&"**/buildcache".to_string()));
    }

    #[test]
    fn missing_root_still_yields_policy_defaults() {
        // `read_dir` failure degrades to the code-owned policy, never an empty
        // exclude that would let a server index a dependency tree.
        let globs = lsp_exclude_globs(Path::new("/nonexistent/symora/root"));
        assert!(globs.contains(&"**/.*".to_string()));
        assert!(globs.contains(&"**/node_modules".to_string()));
    }
}
