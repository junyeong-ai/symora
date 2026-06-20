//! LSP `exclude`-style globs derived from the project's ignore policy.
//!
//! A language server's workspace scan and Symora's native index must agree on
//! which files exist (CLAUDE.md invariant 3). When they disagree, `refs` /
//! `impact` count call sites in directories the index never walked — a git
//! worktree a host project parks under a dotted dir, a vendored copy — and
//! inflate the very coverage numbers an agent trusts. So the `exclude` handed
//! to a server is derived from the [`FileFilter`] policy the index uses, never
//! a hand-maintained per-language literal that drifts from it.
//!
//! The projection mirrors the three tiers `FileFilter::is_ignored` applies — the
//! hidden-directory class, the built-in default directories (only when the
//! project ships no `.gitignore`, exactly as the index gates them), and the
//! project's own `.gitignore` / `.symora/ignore` directories, found by walking
//! the tree with the same predicate the index uses (so a directory ignored only
//! by a nested `.gitignore` is excluded too).
//!
//! A name-only rule (`build/`) — or a built-in default — projects to `**/build`
//! so a copy at any depth (or one created later) is excluded, matching the
//! predicate, which ignores the name wherever it sits. An anchored rule
//! (`/build/`, `deep/buried/`) projects to its exact root-relative path
//! (`build`, `deep/buried`) — NOT `**/build` — so the server excludes the same
//! single directory the index does, never a same-named directory elsewhere the
//! index still walks. `FileFilter::dir_name_ignored_anywhere` distinguishes the
//! two, keeping the exclude and the index in exact agreement (invariant 3).

use std::collections::BTreeSet;
use std::path::Path;

use crate::infra::file_filter::{DEFAULT_IGNORE_PATTERNS, FileFilter};

/// Exclude globs projecting [`FileFilter::is_ignored`] into the glob form a
/// language server's `exclude` expects. A name-only ignore becomes `**/<name>`
/// (so a nested or future copy is excluded too); an anchored or negation-carved
/// rule becomes the directory's exact root-relative path, so the server never
/// over-excludes a directory the index still walks. Returned sorted for a stable
/// `initializationOptions` payload.
pub(super) fn lsp_exclude_globs(root: &Path) -> Vec<String> {
    let filter = FileFilter::with_gitignore(root);
    let mut globs: BTreeSet<String> = BTreeSet::new();

    // Tier 1 — every hidden directory, in one glob. Keeps a server out of
    // `.git`, `.venv`, and any `.<tool>/…` working tree (the dotted dir a host
    // project parks git worktrees under). Unconditional, mirroring the index's
    // `include_hidden = false` (and pyright's own default).
    globs.insert("**/.*".to_string());

    // Tier 2 — the built-in default directories, applied ONLY when the project
    // ships no `.gitignore` (`FileFilter::is_ignored` gates them identically on
    // `gitignore.is_none()`: a project with a `.gitignore` is trusted to declare
    // its own ignores, so the index walks a tracked `vendor/`/`build/` and the
    // server must not exclude it). Emitted statically so a default dir not yet
    // on disk (a `node_modules` created after init) is still excluded. Names
    // only — file/glob entries (`*.log`, `gradle-wrapper.jar`) carry no source,
    // and hidden names are already covered by Tier 1.
    if !filter.has_gitignore() {
        for &pattern in DEFAULT_IGNORE_PATTERNS {
            if !pattern.contains('.') && !pattern.contains('*') {
                globs.insert(format!("**/{pattern}"));
            }
        }
    }

    // Tier 3 — the project's own ignores (`.gitignore` / `.symora/ignore`),
    // found by walking the tree with the SAME predicate the index walks with,
    // so the two exclude the identical set — including a directory ignored only
    // by a nested `.gitignore`, which a top-level scan would miss. A name-only
    // rule projects to `**/<name>` ONLY when every directory of that name is
    // ignored; if a same-named directory is left un-ignored — by an anchored
    // rule (`/build/`) or a negation (`build/` then `!src/build/`) — the
    // any-depth glob would over-exclude it, so that directory projects to its
    // exact root-relative path, keeping the exclude and the index in lockstep.
    //
    // This is a point-in-time projection of the tree as it exists now; it is
    // re-derived whenever a server (re)starts. The `ignore` crate exposes only a
    // negation COUNT, not the patterns, so a negation carving out a directory
    // that does not yet exist on disk cannot be pre-detected by name — that
    // directory is reconciled on the next (re)start. Detecting it eagerly would
    // require falling back to exact paths for every name-only rule whenever the
    // project has any negation, which would drop `**/<name>` future-copy
    // coverage for the common case (a new `node_modules` deep in the tree) — a
    // strictly worse, less precise trade than this tree-driven decision.
    let mut ignored: Vec<(String, String)> = Vec::new();
    let mut unignored_names: BTreeSet<String> = BTreeSet::new();
    collect_ignored_dirs(&filter, root, root, &mut ignored, &mut unignored_names);
    for (name, relpath) in ignored {
        if filter.dir_name_ignored_anywhere(&name) && !unignored_names.contains(&name) {
            globs.insert(format!("**/{name}"));
        } else {
            globs.insert(relpath);
        }
    }

    globs.into_iter().collect()
}

/// Descend through included directories, recording each ignored directory (its
/// name and exact root-relative path) and the NAMES of directories left
/// un-ignored. The caller turns a name-only ignore into `**/<name>` only when no
/// same-named directory is un-ignored — so an anchored rule (`/build/`) or a
/// negation (`build/` then `!src/build/`) projects to the exact path instead of
/// an over-broad glob. An excluded subtree is recorded whole and never walked
/// into. Reusing [`FileFilter::is_ignored`] is what keeps the LSP exclude and
/// the native index in lockstep. Symlinked directories are not followed —
/// matching the index's own walk and ruling out cycles.
fn collect_ignored_dirs(
    filter: &FileFilter,
    root: &Path,
    dir: &Path,
    ignored: &mut Vec<(String, String)>,
    unignored_names: &mut BTreeSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue; // hidden — Tier 1 covers it; never descend an excluded tree
        }
        if filter.is_ignored(&path) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            ignored.push((name.to_string(), rel));
        } else {
            unignored_names.insert(name.to_string());
            collect_ignored_dirs(filter, root, &path, ignored, unignored_names);
        }
    }
}

// Per-server format adapters. The projection above yields directory globs of
// the form `**/<name>` (and the hidden class `**/.*`); each server's `exclude`
// field expects a different shape, so an adapter transforms the canonical set
// rather than each server re-deriving it. A server whose schema cannot express
// the policy faithfully is deliberately NOT wired — a lossy mapping would
// silently mis-scope its scan, the precise failure invariant 4 forbids:
//   - plain directory *names* (Lua `ignoreDir`, Terraform `ignoreDirectoryNames`,
//     Dart `analysisExcludedFolders`) cannot encode the `**/.*` hidden class;
//   - Scala `excludedPackages` filters Java packages, not directories;
//   - rust-analyzer `files.excludeDirs` takes literal relative paths, not globs,
//     so it can encode neither the `**/.*` hidden class nor a name-only
//     `**/<dir>` rule — and rust-analyzer indexes only the cargo crate graph, so
//     a non-crate ignored directory is never walked in the first place;
//   - clangd has no workspace-scan exclude at all: its index follows
//     `compile_commands.json` and open files, never a directory walk.

/// `**/<name>` → `**/<name>/**`: exclude a directory and its whole subtree.
/// The path-glob form pyright already accepts and PHP (`files.exclude`), Ruby
/// (`indexing.excludedPatterns`), and Java jdtls (`import.exclusions`,
/// Ant-style) share.
pub(super) fn lsp_exclude_subtree_globs(root: &Path) -> Vec<String> {
    lsp_exclude_globs(root)
        .into_iter()
        .map(|glob| format!("{glob}/**"))
        .collect()
}

/// `**/<name>` → `**/<name>/**/*`: OmniSharp's `excludeSearchPatterns` form
/// (matching its sibling `systemExcludeSearchPatterns`).
pub(super) fn lsp_exclude_search_patterns(root: &Path) -> Vec<String> {
    lsp_exclude_globs(root)
        .into_iter()
        .map(|glob| format!("{glob}/**/*"))
        .collect()
}

/// `**/<name>` → `-**/<name>`: gopls `directoryFilters` exclusion form. gopls
/// already skips dotted directories itself (go tooling ignores `.`/`_` dirs),
/// so the `**/.*` hidden class is dropped — emitting it would lean on gopls
/// honoring a `.`-segment glob, which its name-based filter does not promise.
pub(super) fn lsp_exclude_go_directory_filters(root: &Path) -> Vec<String> {
    lsp_exclude_globs(root)
        .into_iter()
        .filter(|glob| glob != "**/.*")
        .map(|glob| format!("-{glob}"))
        .collect()
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
    fn applies_default_directories_without_a_gitignore() {
        // No `.gitignore` → the index's default-pattern fallback fires, so the
        // projection must too.
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
    fn gitignore_present_suppresses_built_in_defaults() {
        // The native index applies no built-in defaults once a `.gitignore`
        // exists (FileFilter::is_ignored gates them on `gitignore.is_none()`),
        // so a tracked, default-named dir (`vendor/`) stays indexed — and the
        // projection must NOT exclude it. Only the project's own ignore does.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "secrets/\n").unwrap();
        fs::create_dir(root.join("vendor")).unwrap();
        fs::create_dir(root.join("secrets")).unwrap();

        let globs = lsp_exclude_globs(root);
        assert!(
            !globs.contains(&"**/vendor".to_string()),
            "a .gitignore'd repo must not force-exclude a tracked default-named dir"
        );
        assert!(globs.contains(&"**/secrets".to_string()));
        assert!(globs.contains(&"**/.*".to_string()));
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
    fn nested_gitignore_directories_are_covered_by_the_walk() {
        // The Tier-3 walk descends `deep/` and finds `buried` even though no
        // top-level scan would — so the LSP exclude and the index agree on it.
        // `/deep/buried/` is path-anchored, so the glob is the exact relative
        // path, NOT `**/buried` (which would over-exclude any `buried` dir).
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "/deep/buried/\n").unwrap();
        fs::create_dir_all(root.join("deep").join("buried")).unwrap();

        let globs = lsp_exclude_globs(root);
        assert!(globs.contains(&"deep/buried".to_string()));
        assert!(!globs.contains(&"**/buried".to_string()));
    }

    #[test]
    fn anchored_root_rule_excludes_only_the_root_dir_not_nested_namesakes() {
        // `/build/` (anchored) ignores ONLY the root `build/`. The projection
        // must be the exact path `build`, never `**/build` — otherwise the
        // server would exclude a tracked, indexed `src/vendor/build/` the index
        // still walks, and `refs`/`impact` would disagree across modes (inv. 3).
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "/build/\n").unwrap();
        fs::create_dir(root.join("build")).unwrap();
        fs::create_dir_all(root.join("src").join("vendor").join("build")).unwrap();

        let globs = lsp_exclude_globs(root);
        assert!(
            globs.contains(&"build".to_string()),
            "an anchored /build/ projects to its exact root-relative path"
        );
        assert!(
            !globs.contains(&"**/build".to_string()),
            "must not over-exclude the indexed nested src/vendor/build"
        );
    }

    #[test]
    fn symora_ignore_directories_are_projected_like_gitignore() {
        // `.symora/ignore` is the second ignore source the index honors; the
        // projection must cover it too (the walk's `is_ignored` and the
        // anchoring probe both consult it), or the exclude would drift from the
        // index on a `.symora/ignore`-only rule.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".symora")).unwrap();
        fs::write(root.join(".symora").join("ignore"), "generated/\n").unwrap();
        fs::create_dir(root.join("generated")).unwrap();

        let globs = lsp_exclude_globs(root);
        assert!(globs.contains(&"**/generated".to_string()));
    }

    #[test]
    fn negated_subdirectory_is_not_over_excluded() {
        // `build/` then `!src/build/`: the index ignores the root `build/` but
        // KEEPS `src/build/` (the negation re-includes it). A name-only
        // `**/build` glob would over-exclude the kept `src/build/`, so the root
        // `build/` must project to its exact path — the server and index then
        // agree that `src/build/` is walked (invariant 3).
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "build/\n!src/build/\n").unwrap();
        fs::create_dir(root.join("build")).unwrap();
        fs::create_dir_all(root.join("src").join("build")).unwrap();

        let globs = lsp_exclude_globs(root);
        assert!(
            globs.contains(&"build".to_string()),
            "the ignored root build/ projects to its exact path"
        );
        assert!(
            !globs.contains(&"**/build".to_string()),
            "must not over-exclude the negation-kept src/build/"
        );
    }

    #[test]
    fn unanchored_rule_excludes_the_name_at_any_depth() {
        // `build/` (no leading slash) ignores `build` wherever it sits — the
        // index ignores a nested one too, so `**/build` is exact, not broad.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir(root.join("build")).unwrap();

        let globs = lsp_exclude_globs(root);
        assert!(globs.contains(&"**/build".to_string()));
        assert!(!globs.contains(&"build".to_string()));
    }

    #[test]
    fn missing_root_still_yields_policy_defaults() {
        // `read_dir` failure (and absent `.gitignore`) degrades to the
        // code-owned defaults, never an empty exclude that would let a server
        // index a dependency tree.
        let globs = lsp_exclude_globs(Path::new("/nonexistent/symora/root"));
        assert!(globs.contains(&"**/.*".to_string()));
        assert!(globs.contains(&"**/node_modules".to_string()));
    }

    #[test]
    fn subtree_adapter_appends_recursive_suffix() {
        let globs = lsp_exclude_subtree_globs(Path::new("/nonexistent/symora/root"));
        assert!(globs.contains(&"**/.*/**".to_string()));
        assert!(globs.contains(&"**/node_modules/**".to_string()));
    }

    #[test]
    fn search_pattern_adapter_matches_omnisharp_form() {
        let globs = lsp_exclude_search_patterns(Path::new("/nonexistent/symora/root"));
        assert!(globs.contains(&"**/.*/**/*".to_string()));
        assert!(globs.contains(&"**/node_modules/**/*".to_string()));
    }

    #[test]
    fn go_directory_filters_negate_and_drop_the_hidden_class() {
        // gopls skips dotted dirs itself, so the hidden class is dropped; every
        // remaining entry is negated. A stray un-negated entry would flip gopls
        // to *include* a dependency tree — the bug this exists to prevent.
        let globs = lsp_exclude_go_directory_filters(Path::new("/nonexistent/symora/root"));
        assert!(!globs.iter().any(|g| g.contains("**/.*")));
        assert!(globs.contains(&"-**/node_modules".to_string()));
        assert!(globs.iter().all(|g| g.starts_with('-')));
    }
}
