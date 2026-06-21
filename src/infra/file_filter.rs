//! The project's file-ignore policy — the single authority for "would the index
//! walk this path?".
//!
//! One predicate, [`FileFilter::is_ignored`], answers that question with full
//! per-directory `.gitignore` semantics (a nested `.gitignore` constrains only
//! its own subtree, exactly as git, ripgrep, and fd resolve it). Discovery does
//! not re-derive the policy: [`FileFilter::discover_files`] drives an
//! [`ignore::WalkBuilder`] as a pure traversal engine and delegates every
//! keep/prune decision back to the same `is_ignored`. Walk and single-path query
//! therefore cannot disagree — they are the same code (invariant 3).
//!
//! ## What counts as the policy
//!
//! Only the project's own, committed ignore sources, so the index is reproducible
//! across machines and CI (a value `pack.rs` shares):
//!
//! - the per-directory `.gitignore` tree (root and nested), resolved with real
//!   git precedence — a deeper file overrides a shallower one, `!` re-includes;
//! - `.symora/ignore`, a root-anchored ignore for symora-only exclusions;
//! - a built-in default set ([`DEFAULT_IGNORE_PATTERNS`]: `node_modules`,
//!   `target`, …) applied ONLY when the project ships no root `.gitignore` — a
//!   project with one is trusted to declare its own ignores, so a tracked
//!   `vendor/`/`build/` stays indexed;
//! - hidden entries (any path component starting with `.`), a hard exclusion
//!   senior to the `.gitignore` rules — a `!`-negation cannot re-include a hidden
//!   path. This is the ripgrep/fd default (a hidden filter independent of the
//!   ignore files, not a git concept), and it is what keeps the walk out of
//!   `.git`, `.symora`, and every dotted working tree unconditionally.
//!
//! Machine-local git state — the global `core.excludesFile` and
//! `.git/info/exclude` — is deliberately NOT consulted: it would make the index
//! depend on per-user, per-clone configuration that is never committed.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::Match;
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// The project's ignore policy, cheap to clone (a shared, lazily-populated
/// matcher cache behind an [`Arc`]).
///
/// A `FileFilter` is a **point-in-time snapshot** of the ignore policy. What it
/// reads from disk is cached for the instance's life and never re-checked: at
/// construction ([`FileFilter::new`]) it captures `.symora/ignore` and whether a
/// root `.gitignore` exists; every `.gitignore`'s *contents* — root and nested
/// alike — are read on first access and then cached. This
/// is correct for the way it is used — built per operation (a discovery, a
/// refresh batch, an exclude projection) and dropped — and mirrors how ripgrep
/// resolves its ignore state once per run. Adding or editing an ignore-source
/// file underneath a live instance is not observed; rebuild with
/// [`FileFilter::new`] after the tree's ignore files change. (Ordinary source
/// files appearing or disappearing *are* handled correctly — only the ignore
/// sources themselves are snapshotted.)
#[derive(Clone)]
pub struct FileFilter {
    inner: Arc<Policy>,
}

/// The matchers and per-directory cache backing [`FileFilter`]. Shared by every
/// clone so the `.gitignore` of a directory is parsed at most once per process,
/// whether the query came from the walk or a single-path check.
struct Policy {
    root: PathBuf,
    /// `.symora/ignore`, rooted at the project root (a root-anchored source).
    symora_ignore: Option<Gitignore>,
    /// Whether the built-in [`DEFAULT_IGNORE_PATTERNS`] apply — true exactly when
    /// the project ships no root `.gitignore`.
    apply_defaults: bool,
    /// `dir -> compiled .gitignore for that dir` (None = the dir has none),
    /// populated lazily so each directory's file is parsed at most once. The
    /// root `.gitignore` is just another entry, keyed by the root path.
    gitignore_cache: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
    /// `directory -> is it ignored by its ancestors' rules` (see
    /// [`Policy::element_ignored`]). The top-down walk re-derives every ancestor
    /// directory of every file, so memoizing them collapses discovery from
    /// O(depth²) to ~O(depth) per entry on deep trees — a directory's verdict is
    /// computed once and reused by all its descendants. Only directories are
    /// stored: a file is a leaf, computed once, and never re-queried as an
    /// ancestor. Caching solely directory (`is_dir == true`) verdicts also keeps
    /// the key sound — the verdict depends on `is_dir` (a `build/` rule matches
    /// only directories), so a file query never reads a same-named directory's
    /// answer. Keyed by path alone, which allows borrow-based, allocation-free
    /// lookups.
    verdict_cache: Mutex<HashMap<PathBuf, bool>>,
}

impl FileFilter {
    /// Build the ignore policy for `root` — a point-in-time snapshot of the
    /// tree's ignore-source files (see the type doc); rebuild after they change.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let symora_ignore = load_symora_ignore(&root);
        let apply_defaults = !root.join(".gitignore").is_file();
        Self {
            inner: Arc::new(Policy {
                root,
                symora_ignore,
                apply_defaults,
                gitignore_cache: Mutex::new(HashMap::new()),
                verdict_cache: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Whether `path` is excluded by the policy. Stats the path to learn whether
    /// it is a directory (a `build/` rule matches directories only); a path that
    /// does not exist on disk is judged as a file. Discovery never relies on this
    /// stat — its walk threads the type the directory iterator already knows
    /// straight to the shared predicate, so the two never diverge for a real
    /// entry.
    pub fn is_ignored(&self, path: &Path) -> bool {
        self.inner.is_ignored(path, path.is_dir())
    }

    /// Every file under the root the policy keeps. With a non-empty `extensions`
    /// list only those extensions are returned; an empty list keeps every kept
    /// file regardless of extension.
    ///
    /// The `ignore` crate is used purely to traverse and prune — all of its own
    /// ignore handling is off, and `filter_entry` routes each entry through
    /// [`Self::is_ignored`], so an ignored directory is pruned (never descended)
    /// and the walk and the predicate stay identical by construction.
    pub fn discover_files(&self, extensions: &[&str]) -> Vec<PathBuf> {
        let policy = self.inner.clone();
        let walker = WalkBuilder::new(&self.inner.root)
            // The crate is a traversal engine only: disable every built-in
            // filter (hidden, .ignore, all git ignore sources, parents) so the
            // policy is decided in exactly one place — `is_ignored`, via
            // `filter_entry`. An ignored directory returns false and is pruned,
            // never descended, matching git's "don't walk an ignored tree".
            .standard_filters(false)
            .filter_entry(move |entry| {
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                !policy.is_ignored(entry.path(), is_dir)
            })
            .build();

        let mut files = Vec::new();
        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            if !extensions.is_empty() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !extensions.contains(&ext) {
                    continue;
                }
            }
            files.push(path.to_path_buf());
        }
        files
    }

    /// Whether the built-in defaults are suppressed because the project declares
    /// its own ignores — i.e. a root `.gitignore` is present. The LSP-exclude
    /// projection gates its default-directory tier on this exactly as
    /// [`Self::is_ignored`] does, so the server and the index agree on whether a
    /// tracked `vendor/`/`build/` is walked.
    pub(crate) fn has_gitignore(&self) -> bool {
        !self.inner.apply_defaults
    }

    /// Whether a directory of this NAME is ignored wherever it sits — by a
    /// tree-wide rule (an unanchored `build/` in the root `.gitignore` /
    /// `.symora/ignore`, or a built-in default) — as opposed to only at a
    /// specific path (an anchored `/build/`, or a rule in a *nested* `.gitignore`
    /// that binds only its own subtree).
    ///
    /// The LSP-exclude projection uses this to know whether `**/<name>` matches
    /// the same set [`Self::is_ignored`] does (`true` → the glob is exact;
    /// `false` → the ignore is local and must project to an exact path, or the
    /// server would over-exclude a same-named directory the index still walks —
    /// invariant 3). Only root-anchored sources are consulted, so a `src/` rule
    /// living in a nested `pkg/.gitignore` never makes `src` look ignored
    /// everywhere.
    pub(crate) fn dir_name_ignored_anywhere(&self, name: &str) -> bool {
        self.inner.dir_name_ignored_anywhere(name)
    }
}

impl Policy {
    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false; // outside the project — not ours to judge
        };
        if rel.as_os_str().is_empty() {
            return false; // the root itself
        }

        // Hidden entries (`.git`, `.symora`, `.github`, …) are a hard exclusion,
        // checked before any `.gitignore`: a `!`-negation can never re-include a
        // hidden path. This is the ripgrep/fd default (hidden is a filter
        // independent of, and senior to, the ignore files), not git's — git has
        // no notion of hidden. It is what guarantees the walk never enters `.git`
        // or `.symora` regardless of project rules.
        if rel.components().any(is_hidden_component) {
            return true;
        }

        // Walk the path's elements from the shallowest down to the leaf. The
        // first ignored element wins, so an ignored ANCESTOR directory excludes
        // the whole path — and a `.gitignore` *inside* that ignored directory is
        // never consulted. This is git's rule that a parent's exclusion cannot be
        // undone by a negation deeper in the tree, and it is what keeps this
        // predicate identical to `discover_files`, which prunes the ignored
        // ancestor before it ever descends to read the nested file.
        let components: Vec<Component<'_>> = rel.components().collect();
        let last = components.len().saturating_sub(1);
        let mut element = self.root.clone();
        for (depth, component) in components.iter().enumerate() {
            element.push(component);
            let element_is_dir = depth < last || is_dir;
            if self.element_ignored(&element, element_is_dir) {
                return true;
            }
        }
        false
    }

    /// [`Self::compute_element_ignored`], with directory verdicts memoized so a
    /// directory's answer is reused by every descendant the top-down walk later
    /// visits.
    fn element_ignored(&self, element: &Path, is_dir: bool) -> bool {
        // Files bypass the cache. They are leaves — never re-queried as an
        // ancestor — so caching them buys nothing, and skipping them keeps every
        // stored verdict an `is_dir == true` answer. That is what makes the
        // path-only key sound: a file query (`is_dir == false`) can never read a
        // same-named directory's verdict, and the two genuinely differ for a
        // directory-only rule like `build/`.
        if !is_dir {
            return self.compute_element_ignored(element, false);
        }
        if let Some(&cached) = self.verdict_cache.lock().unwrap().get(element) {
            return cached;
        }
        let verdict = self.compute_element_ignored(element, true);
        self.verdict_cache
            .lock()
            .unwrap()
            .insert(element.to_path_buf(), verdict);
        verdict
    }

    /// Whether a single path element is excluded by the `.gitignore` files of its
    /// ancestor directories (deepest first — a deeper file overrides a shallower
    /// one and a `!` re-includes), then `.symora/ignore`, then the built-in
    /// defaults. Parent directories are handled by the caller's top-down walk, so
    /// each source is matched against this element exactly, never its parents. A
    /// whitelist or no match means "not excluded here" — the caller keeps
    /// descending.
    fn compute_element_ignored(&self, element: &Path, is_dir: bool) -> bool {
        for dir in self.ancestor_dirs(element) {
            if let Some(gitignore) = self.dir_gitignore(dir) {
                match gitignore.matched(element, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
        }
        if let Some(gitignore) = &self.symora_ignore {
            match gitignore.matched(element, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        self.apply_defaults
            && element
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(matches_default_pattern)
    }

    fn dir_name_ignored_anywhere(&self, name: &str) -> bool {
        // Probe the name nested under a synthetic, never-ignored parent: a
        // tree-wide rule (`build/`, no slash) still matches the relocated probe;
        // an anchored rule (`/build/`) does not, because the probe's first
        // component is the synthetic anchor, not `build`.
        let probe = Path::new("__symora_anchor_probe__").join(name);
        let probes_ignored =
            |gitignore: &Gitignore| matches!(gitignore.matched(&probe, true), Match::Ignore(_));
        if self
            .dir_gitignore(&self.root)
            .is_some_and(|g| probes_ignored(&g))
        {
            return true;
        }
        if self.symora_ignore.as_ref().is_some_and(probes_ignored) {
            return true;
        }
        self.apply_defaults && matches_default_pattern(name)
    }

    /// The directories from `path`'s parent up to and including the root, deepest
    /// first — the chain whose `.gitignore`s govern `path`. A directory's own
    /// `.gitignore` never decides its own fate, only its parent's does, so the
    /// chain starts at the parent.
    fn ancestor_dirs<'a>(&self, path: &'a Path) -> Vec<&'a Path> {
        let mut dirs = Vec::new();
        let mut current = path.parent();
        while let Some(dir) = current {
            if !dir.starts_with(&self.root) {
                break; // above the root
            }
            dirs.push(dir);
            if dir == self.root {
                break;
            }
            current = dir.parent();
        }
        dirs
    }

    /// The compiled `.gitignore` for `dir`, parsed once and cached. `None` when
    /// the directory has no `.gitignore`.
    fn dir_gitignore(&self, dir: &Path) -> Option<Arc<Gitignore>> {
        if let Some(cached) = self.gitignore_cache.lock().unwrap().get(dir) {
            return cached.clone();
        }
        // Parse outside the lock — never hold it across file I/O.
        let built = load_dir_gitignore(dir);
        self.gitignore_cache
            .lock()
            .unwrap()
            .insert(dir.to_path_buf(), built.clone());
        built
    }
}

/// Build the matcher for `.symora/ignore`, rooted at the project root so its
/// patterns are anchored there (a single root-level ignore file, not nested).
fn load_symora_ignore(root: &Path) -> Option<Gitignore> {
    let path = root.join(".symora").join("ignore");
    if !path.is_file() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(root);
    if let Some(err) = builder.add(&path) {
        tracing::warn!("Failed to parse {}: {err}", path.display());
    }
    builder.build().ok()
}

/// Build the matcher for `dir/.gitignore`, rooted at `dir` so its patterns bind
/// `dir`'s subtree — the per-directory anchoring git applies.
fn load_dir_gitignore(dir: &Path) -> Option<Arc<Gitignore>> {
    let path = dir.join(".gitignore");
    if !path.is_file() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(dir);
    if let Some(err) = builder.add(&path) {
        tracing::warn!("Failed to parse {}: {err}", path.display());
    }
    builder.build().ok().map(Arc::new)
}

fn is_hidden_component(component: Component<'_>) -> bool {
    matches!(component, Component::Normal(n) if n.to_str().is_some_and(|s| s.starts_with('.')))
}

/// Whether a single path *component* name matches the default ignore set. A
/// directory/file-name matcher, not a full-path substring matcher — callers
/// apply it per component so a file named `targets.rs` is not caught by `target`.
pub(crate) fn matches_default_pattern(name: &str) -> bool {
    DEFAULT_IGNORE_PATTERNS.iter().any(|p| {
        if let Some(suffix) = p.strip_prefix('*') {
            name.ends_with(suffix)
        } else {
            name == *p
        }
    })
}

pub(crate) const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    // Dependencies
    "node_modules",
    "vendor",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".pnp",
    ".yarn",
    // Build outputs
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".output",
    "coverage",
    // Gradle/Maven
    ".gradle",
    ".m2",
    "gradle-wrapper.jar",
    // Cache directories
    ".cache",
    ".parcel-cache",
    ".turbo",
    ".eslintcache",
    ".prettiercache",
    // Generated code
    "generated",
    "gen",
    ".generated",
    // Test artifacts
    ".pytest_cache",
    ".tox",
    "htmlcov",
    // Logs
    "logs",
    "*.log",
    // Temporary
    "tmp",
    "temp",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        write(path, "");
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn root_gitignore_excludes_named_directory() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "*.log\ntarget/\n").unwrap();
        touch(&root.join("main.rs"));
        touch(&root.join("debug.log"));
        fs::create_dir(root.join("target")).unwrap();
        touch(&root.join("target/app"));

        let filter = FileFilter::new(root);
        assert!(!filter.is_ignored(&root.join("main.rs")));
        assert!(filter.is_ignored(&root.join("debug.log")));
        assert!(filter.is_ignored(&root.join("target")));
        assert!(filter.is_ignored(&root.join("target/app")));
    }

    #[test]
    fn nested_gitignore_binds_only_its_own_subtree() {
        // The regression that started this: an unanchored `src/` in a NESTED
        // .gitignore must ignore only that package's src/, never every src/ in
        // the repo. fd/git scope it to the nested dir; the index must too.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        write(&root.join("packages/tokens/.gitignore"), "src/\n");
        touch(&root.join("packages/tokens/src/generated.ts"));
        touch(&root.join("apps/web/src/index.ts"));

        let filter = FileFilter::new(root);
        assert!(
            filter.is_ignored(&root.join("packages/tokens/src/generated.ts")),
            "the nested rule must ignore its own src/"
        );
        assert!(
            !filter.is_ignored(&root.join("apps/web/src/index.ts")),
            "a same-named src/ elsewhere must stay indexed"
        );

        let discovered = filter.discover_files(&["ts"]);
        assert!(
            discovered
                .iter()
                .any(|p| p.ends_with("apps/web/src/index.ts"))
        );
        assert!(
            !discovered
                .iter()
                .any(|p| p.ends_with("packages/tokens/src/generated.ts"))
        );
    }

    #[test]
    fn deeper_negation_reincludes() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        write(&root.join("src/.gitignore"), "!build/\n");
        touch(&root.join("build/out.js"));
        touch(&root.join("src/build/keep.js"));

        let filter = FileFilter::new(root);
        assert!(filter.is_ignored(&root.join("build")));
        assert!(!filter.is_ignored(&root.join("src/build")));
        assert!(!filter.is_ignored(&root.join("src/build/keep.js")));
    }

    #[test]
    fn ignored_parent_cannot_be_reincluded_from_within() {
        // git rule: once a directory is excluded, a `!` negation in a
        // `.gitignore` *inside* it has no effect — and the index never even
        // descends to read that nested file. The single-path query MUST agree
        // with the walk: both treat `foo/bar.rs` as excluded.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "foo/\n").unwrap();
        write(&root.join("foo/.gitignore"), "!bar.rs\n");
        touch(&root.join("foo/bar.rs"));
        touch(&root.join("keep.rs"));

        let filter = FileFilter::new(root);
        assert!(
            filter.is_ignored(&root.join("foo/bar.rs")),
            "an ignored parent excludes its contents; the nested !bar.rs is inert"
        );
        assert!(filter.is_ignored(&root.join("foo")));

        let discovered = filter.discover_files(&["rs"]);
        assert!(discovered.iter().any(|p| p.ends_with("keep.rs")));
        assert!(
            !discovered.iter().any(|p| p.ends_with("bar.rs")),
            "discover_files and is_ignored must agree — single authority"
        );
    }

    #[test]
    fn root_negation_under_ignored_dir_is_inert() {
        // `a/` then `!a/b/` at the root: git keeps `a/` (and all of it) ignored —
        // the negation of a path under an excluded directory does not re-include.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "a/\n!a/b/\n").unwrap();
        touch(&root.join("a/b/c.rs"));

        let filter = FileFilter::new(root);
        assert!(filter.is_ignored(&root.join("a")));
        assert!(filter.is_ignored(&root.join("a/b")));
        assert!(filter.is_ignored(&root.join("a/b/c.rs")));
    }

    #[test]
    fn dir_only_rule_survives_a_path_materializing() {
        // A directory-only rule (`build/`) matches a directory but not a
        // non-directory of the same name, so the verdict depends on `is_dir`.
        // Querying the path before it exists (stat → not a directory) must never
        // poison the answer the walk needs once the directory is real — the memo
        // must not share a slot between a file query and a directory query.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        let filter = FileFilter::new(root);

        // `build` does not exist yet → judged as a file → the dir-only rule misses.
        assert!(!filter.is_ignored(&root.join("build")));

        // It materializes as a directory; the SAME instance must now ignore it
        // and its contents, with no stale verdict carried over from the query above.
        fs::create_dir(root.join("build")).unwrap();
        touch(&root.join("build/out.o"));
        assert!(
            filter.is_ignored(&root.join("build")),
            "a dir-only rule must match once the directory exists"
        );
        assert!(filter.is_ignored(&root.join("build/out.o")));
    }

    #[test]
    fn hidden_is_a_hard_exclusion_gitignore_cannot_reinclude() {
        // Intentional, ripgrep/fd-style: hidden entries are filtered ahead of the
        // ignore rules, so a `!`-negation cannot pull a dotted path back in. This
        // is what keeps `.git`/`.symora` out unconditionally; documented so the
        // behavior is a contract, not an accident.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "!.github/\n!.keep.rs\n").unwrap();
        touch(&root.join(".github/workflows/ci.rs"));
        touch(&root.join(".keep.rs"));
        touch(&root.join("visible.rs"));

        let filter = FileFilter::new(root);
        assert!(filter.is_ignored(&root.join(".github/workflows/ci.rs")));
        assert!(filter.is_ignored(&root.join(".keep.rs")));
        assert!(!filter.is_ignored(&root.join("visible.rs")));

        let discovered = filter.discover_files(&["rs"]);
        assert!(discovered.iter().any(|p| p.ends_with("visible.rs")));
        assert!(
            !discovered
                .iter()
                .any(|p| p.ends_with("ci.rs") || p.ends_with(".keep.rs")),
            "hidden paths must never reach discovery"
        );
    }

    #[test]
    fn symora_ignore_is_honored() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir(root.join(".symora")).unwrap();
        fs::write(root.join(".symora/ignore"), "*.test.rs\n").unwrap();
        touch(&root.join("main.rs"));
        touch(&root.join("main.test.rs"));

        let filter = FileFilter::new(root);
        assert!(!filter.is_ignored(&root.join("main.rs")));
        assert!(filter.is_ignored(&root.join("main.test.rs")));
    }

    #[test]
    fn defaults_apply_only_without_a_root_gitignore() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        touch(&root.join("node_modules/pkg/index.js"));
        touch(&root.join("main.rs"));

        let filter = FileFilter::new(root);
        assert!(filter.is_ignored(&root.join("node_modules")));
        assert!(!filter.is_ignored(&root.join("main.rs")));
        assert!(!filter.has_gitignore());

        // A root .gitignore that does NOT mention node_modules → defaults off,
        // the tracked dir is indexed (the project's policy is authoritative).
        fs::write(root.join(".gitignore"), "secrets/\n").unwrap();
        let filter = FileFilter::new(root);
        assert!(!filter.is_ignored(&root.join("node_modules")));
        assert!(filter.has_gitignore());
    }

    #[test]
    fn hidden_entries_are_always_excluded() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(&root.join(".git/config"));
        touch(&root.join(".symora/store.db"));
        touch(&root.join("src/.hidden/x.rs"));
        touch(&root.join("src/main.rs"));

        let filter = FileFilter::new(root);
        assert!(filter.is_ignored(&root.join(".git/config")));
        assert!(filter.is_ignored(&root.join(".symora/store.db")));
        assert!(filter.is_ignored(&root.join("src/.hidden/x.rs")));
        assert!(!filter.is_ignored(&root.join("src/main.rs")));
    }

    #[test]
    fn dir_name_ignored_anywhere_distinguishes_tree_wide_from_local() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "build/\n/dist/\n").unwrap();
        write(&root.join("pkg/.gitignore"), "src/\n");

        let filter = FileFilter::new(root);
        // Unanchored root rule → ignored wherever it sits.
        assert!(filter.dir_name_ignored_anywhere("build"));
        // Anchored root rule → only at its path, not "anywhere".
        assert!(!filter.dir_name_ignored_anywhere("dist"));
        // Rule lives in a nested .gitignore → never tree-wide.
        assert!(!filter.dir_name_ignored_anywhere("src"));
    }

    #[test]
    fn dir_name_ignored_anywhere_covers_defaults_without_gitignore() {
        let temp = TempDir::new().unwrap();
        let filter = FileFilter::new(temp.path());
        assert!(filter.dir_name_ignored_anywhere("node_modules"));
        assert!(filter.dir_name_ignored_anywhere("target"));
        assert!(!filter.dir_name_ignored_anywhere("src"));
    }

    #[test]
    fn discover_respects_extensions_and_prunes_ignored_trees() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        touch(&root.join("a.rs"));
        touch(&root.join("b.ts"));
        touch(&root.join("node_modules/dep/index.js"));

        let filter = FileFilter::new(root);
        let rs = filter.discover_files(&["rs"]);
        assert_eq!(rs.len(), 1);
        assert!(rs[0].ends_with("a.rs"));

        let all = filter.discover_files(&[]);
        assert!(all.iter().any(|p| p.ends_with("a.rs")));
        assert!(all.iter().any(|p| p.ends_with("b.ts")));
        assert!(
            !all.iter()
                .any(|p| p.to_string_lossy().contains("node_modules"))
        );
    }
}
