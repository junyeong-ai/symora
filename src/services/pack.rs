//! Context packs ranked from syntax-derived module paths.
//!
//! Imports are cached per file. Signatures are loaded in rank order and
//! admitted whole within the caller's approximate token budget.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::constants::defaults::PACK_SYMBOLS_PER_FILE;
use crate::infra::file_filter::{Discovery, FileFilter};
use crate::models::config::SearchConfig;
use crate::models::symbol::Language;
use crate::services::imports::{FileImports, ImportExtractor};
use crate::services::pack_cache::{CachedEntry, PackCache};
use crate::utils::estimate_tokens;

/// Tunable knobs for the pack engine.
#[derive(Debug, Clone)]
pub struct PackConfig {
    pub damping: f64,
    pub max_iterations: usize,
    pub convergence_tolerance: f64,
    /// How many top-level symbols per file at most. Caps fan-out before the
    /// token-budget loop has to decide what to drop.
    pub max_symbols_per_file: usize,
    /// Hard cap on file size to read. Larger files are skipped so a single
    /// generated artefact does not dominate the pack.
    pub max_file_size_bytes: u64,
    /// When true, `build_pack` reads/writes `.symora/pack-cache.db` and
    /// skips re-extraction for files whose mtime hasn't changed.
    pub use_cache: bool,
}

impl Default for PackConfig {
    fn default() -> Self {
        Self {
            damping: 0.85,
            max_iterations: 30,
            convergence_tolerance: 1e-6,
            max_symbols_per_file: PACK_SYMBOLS_PER_FILE,
            max_file_size_bytes: SearchConfig::default().max_file_size_bytes(),
            use_cache: true,
        }
    }
}

/// One selected file in the resulting pack.
#[derive(Debug, Clone)]
pub struct PackedFile {
    pub path: PathBuf,
    pub language: Language,
    pub rank: f64,
    pub symbols: Vec<PackedSymbol>,
    /// How many the file states, so a caller reading a shape can tell it from
    /// the whole one.
    pub declared_symbols: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackedSymbol {
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub signature: String,
}

/// Result of a pack run.
#[derive(Debug, Clone)]
pub struct PackResult {
    pub files: Vec<PackedFile>,
    pub estimated_tokens: usize,
    pub graph_size: usize,
    pub unsupported_languages: Vec<Language>,
    pub unsupported_import_languages: Vec<Language>,
    pub rust_relative_imports_omitted: bool,
    /// Paths the walk behind this pack could not read. A pack is a selection,
    /// so a file's absence never meant it does not exist — but `graph_size`
    /// publishes how many the selection chose FROM, and the budget's own
    /// exclusions are disclosed while these would not be.
    pub unreadable_paths: Vec<std::path::PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("No eligible file matches focus '{0}'; use a project-relative path or substring")]
    UnmatchedFocus(String),
}

/// Build a context pack for `root` under the given token budget.
///
/// `focus` is an optional hint — a file path (or substring) that the
/// PageRank should bias towards via personalization. When `None`, every
/// node gets a uniform teleport weight.
pub fn build_pack(
    root: &Path,
    budget_tokens: usize,
    focus: Option<&str>,
    file_filter: &FileFilter,
    cfg: &PackConfig,
) -> Result<PackResult, PackError> {
    if budget_tokens == 0 || cfg.max_symbols_per_file == 0 {
        return Ok(PackResult {
            files: Vec::new(),
            estimated_tokens: 0,
            graph_size: 0,
            unsupported_languages: Vec::new(),
            unsupported_import_languages: Vec::new(),
            rust_relative_imports_omitted: false,
            unreadable_paths: Vec::new(),
        });
    }
    let cache = if cfg.use_cache {
        match PackCache::open(root) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("pack cache disabled — open failed: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut collected = collect_nodes(root, file_filter, cfg, cache.as_ref());
    let nodes = collected.nodes;
    if let Some(focus) = focus
        && (focus.trim().is_empty()
            || !nodes
                .iter()
                .any(|node| node.rel_path.to_lowercase().contains(&focus.to_lowercase())))
    {
        return Err(PackError::UnmatchedFocus(focus.to_string()));
    }

    // Prune evicts what the walk did not find, which only means "gone" when
    // the walk saw everything. A path it could not read is not absent, and
    // dropping its entry throws away work a later, complete walk has to redo.
    if let Some(cache) = cache.as_ref()
        && collected.unreadable.is_empty()
    {
        let active: HashSet<String> = nodes.iter().map(|n| n.rel_path.clone()).collect();
        if let Err(e) = cache.prune(&active) {
            tracing::debug!("pack cache prune failed: {e}");
        }
    }

    let graph = build_import_graph(&nodes, &Manifests::read(root, &nodes));
    let personalization = personalization_vector(&nodes, focus);
    let ranks = page_rank(&graph.edges, &personalization, cfg);
    let fitted = fit_to_budget(
        &nodes,
        &ranks,
        budget_tokens,
        cfg.max_symbols_per_file,
        root,
        cache.as_ref(),
    );
    collected.unreadable.extend(fitted.unreadable_paths);
    let mut unsupported_languages: Vec<Language> = nodes
        .iter()
        .map(|node| node.language)
        .filter(|language| !crate::services::symbols::SymbolExtractor::is_supported(*language))
        .collect();
    unsupported_languages.sort_by_key(|language| language.lsp_id());
    unsupported_languages.dedup();
    let mut unsupported_import_languages: Vec<_> = nodes
        .iter()
        .map(|node| node.language)
        .filter(|language| !ImportExtractor::is_supported(*language))
        .collect();
    unsupported_import_languages.sort_by_key(|language| language.lsp_id());
    unsupported_import_languages.dedup();
    Ok(PackResult {
        unsupported_import_languages,
        unsupported_languages,
        rust_relative_imports_omitted: graph.rust_relative_imports_omitted,
        files: fitted.files,
        estimated_tokens: fitted.estimated_tokens,
        graph_size: nodes.len(),
        unreadable_paths: collected.unreadable,
    })
}

// --- node collection ------------------------------------------------------

#[derive(Debug, Clone)]
struct Node {
    id: usize,
    rel_path: String,
    language: Language,
    module_path: Vec<String>,
    /// Components of the directory holding this file — what a reference to
    /// a package resolves to in languages where a package is a directory.
    directory: Vec<String>,
    imports: FileImports,
    mtime: i64,
    signatures: Option<Vec<PackedSymbol>>,
}

fn collect_nodes(
    root: &Path,
    file_filter: &FileFilter,
    cfg: &PackConfig,
    cache: Option<&PackCache>,
) -> CollectedNodes {
    let mut nodes = Vec::new();
    let Discovery {
        files,
        mut unreadable,
    } = file_filter.discover_files(&[]);
    for abs_path in files {
        let abs_path = abs_path.as_path();
        let language = Language::from_path(abs_path);
        if !language.is_code() {
            continue;
        }
        let metadata = match std::fs::metadata(abs_path) {
            Ok(m) => m,
            Err(e) => {
                if crate::infra::hides_content(&e) {
                    unreadable.push(abs_path.to_path_buf());
                }
                continue;
            }
        };
        if metadata.len() > cfg.max_file_size_bytes {
            continue;
        }
        let Some(mtime) = file_mtime(&metadata) else {
            unreadable.push(abs_path.to_path_buf());
            continue;
        };
        let rel_path = abs_path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| abs_path.display().to_string());
        let module_path = module_path(abs_path, root, language);
        let directory = directory_path(abs_path, root);

        // Cache hit: same mtime + same language → replay cached artefacts.
        if let Some(cache) = cache
            && let Some(cached) = cache.get(&rel_path)
            && cached.mtime == mtime
            && cached.language == language
        {
            nodes.push(Node {
                id: nodes.len(),
                rel_path,
                language,
                module_path,
                directory,
                mtime,
                imports: cached.imports,
                signatures: cached.signatures,
            });
            continue;
        }

        // Cache miss: pay the I/O + extraction cost, then upsert.
        let contents = match std::fs::read_to_string(abs_path) {
            Ok(c) => c,
            Err(e) => {
                if crate::infra::hides_text(&e) {
                    unreadable.push(abs_path.to_path_buf());
                }
                continue;
            }
        };
        let imports = ImportExtractor::shared().extract(&contents, language);
        let signatures = None;

        if let Some(cache) = cache {
            let entry = CachedEntry {
                mtime,
                language,
                imports: imports.clone(),
                signatures: signatures.clone(),
            };
            if let Err(e) = cache.put(&rel_path, &entry) {
                tracing::debug!("pack cache put failed for {rel_path}: {e}");
            }
        }

        nodes.push(Node {
            id: nodes.len(),
            rel_path,
            language,
            module_path,
            directory,
            imports,
            mtime,
            signatures,
        });
    }

    // Canonical node order: discovery yields files in filesystem readdir order,
    // which is machine-dependent. Sort by rel_path — unique among nodes — and renumber
    // so the whole pack (import graph, PageRank, budget fit) is reproducible
    // for a fixed source state, as pack.rs's own contract promises. ids are
    // opaque keys into the graph maps, so renumber-after-sort is safe.
    nodes.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    for (i, node) in nodes.iter_mut().enumerate() {
        node.id = i;
    }
    CollectedNodes { nodes, unreadable }
}

/// The pack's candidate files, and how many paths it could not read — a
/// directory the walk could not enter, and a discovered file whose metadata or
/// contents would not come back. A pack built over a partial walk must not
/// evict cache entries for the paths it never saw, and the active set is built
/// from the nodes it DID make, so a file dropped after discovery would be
/// pruned as though it were gone.
struct CollectedNodes {
    nodes: Vec<Node>,
    unreadable: Vec<std::path::PathBuf>,
}

/// The file's modification time in nanoseconds, or `None` when the filesystem
/// would not give it.
///
/// Not a default: the mtime is the cache's whole freshness test, so a stand-in
/// value would be recorded and then matched by the same stand-in on every
/// later run — a permanent cache hit serving stale imports and signatures for
/// a file that keeps changing, with nothing saying so. Nanoseconds because at
/// whole-second resolution two edits inside one second are indistinguishable,
/// and the second one is invisible until some later edit happens to land in a
/// different second.
fn file_mtime(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
}

/// Components of the directory holding a file.
///
/// Derived from the file's own location rather than from its module path,
/// which a directory module's `mod`/`index` file has already been folded
/// into: `src/cli/mod.rs` lives in `src/cli` and answers to `src::cli`, and
/// deriving one from the other would place it a level too high.
fn directory_path(abs_path: &Path, root: &Path) -> Vec<String> {
    let rel = abs_path.strip_prefix(root).unwrap_or(abs_path);
    rel.parent()
        .map(|dir| {
            dir.iter()
                .filter_map(|c| c.to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The name a file's language reserves for the file that stands for its
/// whole directory, if the language reserves one.
///
/// Each name belongs to one language family and is an ordinary module
/// elsewhere: Rust addresses `src/cli/` through `mod.rs` while `index.rs`
/// is just a module named `index`, and the reverse holds for JavaScript.
/// Collapsing both everywhere gave a Rust directory holding `mod.rs` and
/// `index.rs` two files with one module path, and an ambiguous path names
/// neither — so the pair silently cost each other every edge.
fn directory_module_file(language: Language) -> Option<&'static str> {
    match language {
        Language::Rust => Some("mod"),
        Language::Python => Some("__init__"),
        Language::JavaScript | Language::TypeScript | Language::Vue => Some("index"),
        _ => None,
    }
}

/// The module path a file answers to, as path components with the file
/// extension dropped and a directory module's file collapsed onto its
/// directory.
///
/// This is the correspondence every language pack understands shares
/// between a module reference and a file location — `cli::call_graph` for
/// `src/cli/call_graph.rs`, `foo.bar` for `foo/bar.py`, `./services/pack`
/// for `src/services/pack.ts`. Resolution matches against it exactly, so
/// nothing here needs to know which prefixes a language's source root
/// conventionally carries.
fn module_path(abs_path: &Path, root: &Path, language: Language) -> Vec<String> {
    let rel = abs_path.strip_prefix(root).unwrap_or(abs_path);
    let mut components: Vec<String> = rel
        .iter()
        .filter_map(|c| c.to_str().map(str::to_string))
        .collect();

    if let Some(last) = components.last_mut()
        && let Some((stem, _extension)) = last.rsplit_once('.')
    {
        *last = stem.to_string();
    }
    if components
        .last()
        .is_some_and(|c| directory_module_file(language) == Some(c.as_str()))
    {
        components.pop();
    }

    components
}

// --- graph -----------------------------------------------------------------

// BTreeMap, not HashMap: PageRank accumulates f64 scores by iterating this
// graph, and float addition is non-associative — a random HashMap iteration
// order would make scores differ by a ULP run-to-run, breaking pack's
// reproducibility contract. Ascending-id iteration is the canonical order.
type Graph = BTreeMap<usize, Vec<usize>>;

struct ImportGraph {
    edges: Graph,
    rust_relative_imports_omitted: bool,
}

fn build_import_graph(nodes: &[Node], manifests: &Manifests) -> ImportGraph {
    let mut exact = PathIndex::default();
    for node in nodes {
        exact
            .by_module
            .entry(node.module_path.as_slice())
            .or_default()
            .push(node.id);
        exact
            .by_directory
            .entry(node.directory.as_slice())
            .or_default()
            .push(node.id);
    }
    let index = ProjectIndex {
        nodes,
        exact,
        manifests,
        python: PythonPath::of(nodes),
        jvm: JvmIndex::of(nodes),
    };

    let mut graph: Graph = BTreeMap::new();
    let mut rust_relative_imports_omitted = false;
    for node in nodes {
        graph.entry(node.id).or_default();
        let importer = Importer::of(node, manifests);
        let mut seen = HashSet::new();
        for target in &node.imports.references {
            let reference = Reference::parse(target, node.language);
            let files = match index.resolve(&reference, &importer) {
                Resolution::Files(files) => files,
                Resolution::NeedsOwnership => {
                    rust_relative_imports_omitted = true;
                    continue;
                }
            };
            for dst in files {
                if dst != node.id && seen.insert(dst) {
                    graph.entry(node.id).or_default().push(dst);
                }
            }
        }
    }
    ImportGraph {
        edges: graph,
        rust_relative_imports_omitted,
    }
}

/// What a reference resolves to.
enum Resolution {
    /// The files it names: none where it names nothing in the project, or
    /// nothing unambiguous.
    Files(Vec<usize>),
    /// A Rust path whose file only module ownership, which pack does not
    /// hold, would determine.
    NeedsOwnership,
}

/// What resolving a file's references needs to know about where the file
/// sits, worked out once for the file rather than once per reference.
struct Importer<'a> {
    node: &'a Node,
    /// The directory a `crate::` path written in a Rust file starts at, where
    /// the package layout determines it.
    crate_root: Option<&'a [String]>,
}

impl<'a> Importer<'a> {
    fn of(node: &'a Node, manifests: &'a Manifests) -> Self {
        let crate_root = if node.language == Language::Rust {
            manifests.rust_crate_root(node)
        } else {
            None
        };
        Self { node, crate_root }
    }
}

/// What a set of candidate files names: one file, none, or several — which
/// name none of them, since an edge to each would hand every one of them the
/// same borrowed rank.
enum Found {
    One(usize),
    Nothing,
    Several,
}

impl Found {
    fn of(candidates: impl IntoIterator<Item = usize>) -> Self {
        let distinct: BTreeSet<usize> = candidates.into_iter().collect();
        let mut distinct = distinct.into_iter();
        match (distinct.next(), distinct.next()) {
            (None, _) => Self::Nothing,
            (Some(one), None) => Self::One(one),
            _ => Self::Several,
        }
    }

    /// The file the first attempt that finds anything names, none where that
    /// attempt finds several.
    fn first(attempts: impl IntoIterator<Item = Found>) -> Vec<usize> {
        for found in attempts {
            match found {
                Self::One(file) => return vec![file],
                Self::Several => return Vec::new(),
                Self::Nothing => {}
            }
        }
        Vec::new()
    }
}

/// Syntactic reference anchors. Filesystem-relative paths retain their hops;
/// an absolute name is read as its language reads it, through what the tree
/// and its manifests declare. Rust paths relative to a module require
/// ownership not held here.
#[derive(Debug, PartialEq, Eq)]
enum Reference {
    /// Relative to the language's source/module context, retaining parent hops.
    Relative { up: usize, components: Vec<String> },
    /// An absolute module name, possibly qualified by its crate or package.
    Absolute(Vec<String>),
    /// A Rust path from the root of the crate writing it (`crate::a::b`).
    Crate(Vec<String>),
    /// A JVM import of all a package or class holds (`import a.b.*`).
    Wildcard(Vec<String>),
    /// A Dart `package:` URI: a file under the named package's `lib`.
    Package {
        package: String,
        components: Vec<String>,
    },
    /// A reference that names nothing in the project, or that is spelled in a
    /// form whose value is not decoded here.
    Unresolved,
}

impl Reference {
    fn parse(raw: &str, language: Language) -> Self {
        if language == Language::Rust {
            return Self::rust(raw);
        }
        if language == Language::Python {
            let dots = raw.bytes().take_while(|byte| *byte == b'.').count();
            let components = raw[dots..]
                .split('.')
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect();
            return if dots > 0 {
                Self::Relative {
                    up: dots - 1,
                    components,
                }
            } else {
                Self::Absolute(components)
            };
        }
        if language == Language::Dart {
            return Self::dart(raw);
        }
        if is_jvm(language) {
            let (name, wildcard) = match raw.strip_suffix(".*") {
                Some(name) => (name, true),
                None => (raw, false),
            };
            let components = name
                .split('.')
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect();
            return if wildcard {
                Self::Wildcard(components)
            } else {
                Self::Absolute(components)
            };
        }
        let mut rest = raw;
        let mut up = 0;
        let mut relative = false;
        loop {
            if let Some(tail) = rest.strip_prefix("../") {
                up += 1;
                relative = true;
                rest = tail;
            } else if let Some(tail) = rest.strip_prefix("./") {
                relative = true;
                rest = tail;
            } else {
                break;
            }
        }
        let mut components = split_components(rest);
        if matches!(
            language,
            Language::JavaScript | Language::TypeScript | Language::Vue
        ) && let Some(last) = components.last_mut()
            && let Some((stem, extension)) = last.rsplit_once('.')
            && matches!(
                extension,
                "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "vue"
            )
        {
            *last = stem.to_string();
        }
        if relative {
            Self::Relative { up, components }
        } else {
            Self::Absolute(components)
        }
    }

    /// A Rust use path: `self` and `super` hops are relative to the module
    /// writing them, `crate` starts at the root of its crate, and any other
    /// first segment names an extern crate or a name in scope.
    fn rust(path: &str) -> Self {
        let segments: Vec<&str> = path.split("::").filter(|part| !part.is_empty()).collect();
        let owned = |segments: &[&str]| segments.iter().map(|part| part.to_string()).collect();
        match segments.split_first() {
            Some((&"crate", rest)) => Self::Crate(owned(rest)),
            Some((&("self" | "super"), _)) => {
                let hops = segments
                    .iter()
                    .take_while(|part| matches!(**part, "self" | "super"))
                    .count();
                Self::Relative {
                    up: segments[..hops]
                        .iter()
                        .filter(|part| **part == "super")
                        .count(),
                    components: owned(&segments[hops..]),
                }
            }
            _ => Self::Absolute(owned(&segments)),
        }
    }

    /// A Dart URI, which the language resolves against the URI of the library
    /// it is written in: one without a scheme is relative to the importing
    /// file whether or not it starts with `./`, a `package:` URI names a file
    /// under that package's `lib`, and any other scheme (`dart:`, `http:`)
    /// names nothing in the project. A URI names a file by its whole name, so
    /// one that does not end in `.dart` names no Dart file, and one carrying
    /// percent-encoding, a query or a fragment is not decoded here.
    fn dart(uri: &str) -> Self {
        if uri.contains(['%', '?', '#']) || uri.starts_with('/') {
            return Self::Unresolved;
        }
        let scheme = uri.split_once(':').filter(|(scheme, _)| {
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        });
        let (package, path) = match scheme {
            Some(("package", rest)) => match rest.split_once('/') {
                Some((package, path)) => (Some(package), path),
                None => return Self::Unresolved,
            },
            Some(_) => return Self::Unresolved,
            None => (None, uri),
        };
        let mut up = 0;
        let mut components: Vec<String> = Vec::new();
        for segment in path.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if components.pop().is_none() {
                        up += 1;
                    }
                }
                _ => components.push(segment.to_string()),
            }
        }
        let Some(stem) = components
            .pop()
            .and_then(|last| last.strip_suffix(".dart").map(str::to_string))
        else {
            return Self::Unresolved;
        };
        components.push(stem);
        match package {
            // A `package:` path that climbs out of `lib` names no file in it.
            Some(_) if up > 0 => Self::Unresolved,
            Some(package) => Self::Package {
                package: package.to_string(),
                components,
            },
            None => Self::Relative { up, components },
        }
    }
}

fn split_components(raw: &str) -> Vec<String> {
    raw.split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Default)]
struct PathIndex<'a> {
    by_module: HashMap<&'a [String], Vec<usize>>,
    by_directory: HashMap<&'a [String], Vec<usize>>,
}

/// The project's files by the names a reference can give them. Every lookup
/// is exact: a module path a reference only ends with is some other module's.
struct ProjectIndex<'a> {
    nodes: &'a [Node],
    exact: PathIndex<'a>,
    manifests: &'a Manifests,
    python: PythonPath<'a>,
    jvm: JvmIndex<'a>,
}

/// What a project's JVM files declare themselves to be. A file's class is
/// named by the package its declaration states and the file's own name,
/// wherever the file sits; its source root is its directory without the path
/// that package spells, where the directory ends in that path.
struct JvmIndex<'a> {
    by_class: HashMap<Vec<String>, Vec<usize>>,
    by_package: HashMap<Vec<String>, Vec<usize>>,
    roots: HashMap<usize, &'a [String]>,
}

impl<'a> JvmIndex<'a> {
    fn of(nodes: &'a [Node]) -> Self {
        let mut index = Self {
            by_class: HashMap::new(),
            by_package: HashMap::new(),
            roots: HashMap::new(),
        };
        for node in nodes.iter().filter(|node| is_jvm(node.language)) {
            let package: Vec<String> = node
                .imports
                .package
                .as_deref()
                .map(|package| package.split('.').map(str::to_string).collect())
                .unwrap_or_default();
            if let Some(root) = node.directory.strip_suffix(package.as_slice()) {
                index.roots.insert(node.id, root);
            }
            if let Some(stem) = node.module_path.last() {
                let class = [package.as_slice(), std::slice::from_ref(stem)].concat();
                index.by_class.entry(class).or_default().push(node.id);
            }
            index.by_package.entry(package).or_default().push(node.id);
        }
        index
    }
}

/// The directories a Python tree puts on `sys.path`, as far as the tree says:
/// a package is a directory holding `__init__.py`, and the directory above a
/// top-level package is where it is imported from.
struct PythonPath<'a> {
    packages: HashSet<&'a [String]>,
    /// The directory above each top-level package, in path order.
    roots: Vec<&'a [String]>,
}

impl<'a> PythonPath<'a> {
    fn of(nodes: &'a [Node]) -> Self {
        let packages: HashSet<&[String]> = nodes
            .iter()
            .filter(|node| {
                node.language == Language::Python
                    && Path::new(&node.rel_path)
                        .file_stem()
                        .is_some_and(|stem| stem == "__init__")
            })
            .map(|node| node.directory.as_slice())
            .collect();
        let mut roots: Vec<&[String]> = packages
            .iter()
            .filter_map(|package| package.split_last().map(|(_, parent)| parent))
            .filter(|parent| !packages.contains(parent))
            .collect();
        roots.sort();
        roots.dedup();
        Self { packages, roots }
    }

    /// The directory `sys.path` holds for a file in `directory`: the nearest
    /// one at or above it that is no package — where a script runs from, or
    /// where its top-level package is imported from.
    fn base_of(&self, directory: &'a [String]) -> Option<&'a [String]> {
        (0..=directory.len())
            .rev()
            .map(|depth| &directory[..depth])
            .find(|directory| !self.packages.contains(directory))
    }
}

impl ProjectIndex<'_> {
    /// What a reference names, nothing where it names something outside the
    /// project or nothing unambiguous.
    fn resolve(&self, reference: &Reference, importer: &Importer) -> Resolution {
        let from = importer.node;
        let files = match reference {
            Reference::Relative { .. } if from.language == Language::Rust => {
                return Resolution::NeedsOwnership;
            }
            Reference::Relative { up, components } => {
                let base = from.directory.len().checked_sub(*up);
                let Some(base) = base else {
                    return Resolution::Files(Vec::new());
                };
                let mut path = from.directory[..base].to_vec();
                path.extend_from_slice(components);
                if package_is_directory(from.language) {
                    self.package(&path, from.language)
                } else {
                    self.module(&path, from.language)
                }
            }
            Reference::Package {
                package,
                components,
            } => match self.manifests.dart_package(package, from) {
                Some(directory) => {
                    let path = [directory, &["lib".to_string()], components].concat();
                    self.module(&path, from.language)
                }
                None => Vec::new(),
            },
            Reference::Unresolved => Vec::new(),
            Reference::Absolute(components) if from.language == Language::Go => self
                .manifests
                .go_package(components, from)
                .map(|directory| self.package(&directory, Language::Go))
                .unwrap_or_default(),
            Reference::Absolute(components) if from.language == Language::Python => {
                self.python_module(components, from)
            }
            Reference::Absolute(name) if is_jvm(from.language) => self.jvm_class(name, from),
            Reference::Wildcard(name) => self.jvm_wildcard(name, from),
            Reference::Crate(path) => match importer.crate_root {
                Some(root) => self.rust_module(root, path),
                None => return Resolution::NeedsOwnership,
            },
            Reference::Absolute(path) if from.language == Language::Rust => {
                return self.rust_path(path, from);
            }
            // Anywhere else an absolute name (a JavaScript package specifier,
            // a Terraform registry source) names what the project's
            // dependencies provide, never a project file by its spelling.
            Reference::Absolute(_) => Vec::new(),
        };
        Resolution::Files(files)
    }

    /// What a Rust path from a name, rather than from `crate`, `self` or
    /// `super`, names. Through a module the file declares (`mod config; use
    /// config::Settings;`) it is relative to the module writing it, which only
    /// module ownership places; from a project library's name it starts at
    /// that library's root; any other name is an extern crate's, which names
    /// nothing in the project and leaves nothing out of its graph.
    fn rust_path(&self, path: &[String], from: &Node) -> Resolution {
        let Some((name, path)) = path.split_first() else {
            return Resolution::Files(Vec::new());
        };
        if from.imports.modules.contains(name) {
            return Resolution::NeedsOwnership;
        }
        Resolution::Files(
            self.manifests
                .rust_library(name, from)
                .map(|root| self.rust_module(root, path))
                .unwrap_or_default(),
        )
    }

    /// The module a Rust path below a crate's root names: the file of that
    /// module path, read whole and then without its last segment, which
    /// names an item in the module where it is not a module itself.
    fn rust_module(&self, root: &[String], path: &[String]) -> Vec<usize> {
        let parent = path.split_last().map(|(_, parent)| parent);
        Found::first(
            std::iter::once(path)
                .chain(parent)
                .filter(|candidate| !candidate.is_empty())
                .map(|candidate| {
                    self.modules_at([[root, candidate].concat()], |language| {
                        language == Language::Rust
                    })
                }),
        )
    }

    /// The file an import of a JVM class names: the one whose declared package
    /// and name spell the class — read whole, then without each trailing
    /// segment in turn, which nested classes and members add to the class
    /// their file declares. Scala imports a package by its name as well,
    /// which names the files the package holds.
    fn jvm_class(&self, name: &[String], from: &Node) -> Vec<usize> {
        for length in (1..=name.len()).rev() {
            if let Some(files) = self.jvm.by_class.get(&name[..length]) {
                return self.jvm_one(files, from).into_iter().collect();
            }
        }
        if from.language == Language::Scala {
            return self.jvm_package(name, from);
        }
        Vec::new()
    }

    /// The files `import a.b.*` names: the class `a.b`, whose members it
    /// imports, or else everything the package `a.b` holds.
    fn jvm_wildcard(&self, name: &[String], from: &Node) -> Vec<usize> {
        match self.jvm.by_class.get(name) {
            Some(files) => self.jvm_one(files, from).into_iter().collect(),
            None => self.jvm_package(name, from),
        }
    }

    /// The one file of several declaring a class that an importer means: the
    /// only one, or the only one in the importer's own source root.
    fn jvm_one(&self, files: &[usize], from: &Node) -> Option<usize> {
        if let [only] = files {
            return Some(*only);
        }
        let own = self.jvm.roots.get(&from.id)?;
        let mut mine = files
            .iter()
            .filter(|id| self.jvm.roots.get(id) == Some(own));
        match (mine.next(), mine.next()) {
            (Some(one), None) => Some(*one),
            _ => None,
        }
    }

    /// The files an import of a whole JVM package names: those declaring it
    /// in the importer's own source root where it has any there — main code
    /// does not see the test sources that share its packages — and otherwise
    /// those of the one directory in the project that holds the package.
    fn jvm_package(&self, package: &[String], from: &Node) -> Vec<usize> {
        let Some(files) = self.jvm.by_package.get(package) else {
            return Vec::new();
        };
        if let Some(own) = self.jvm.roots.get(&from.id) {
            let mine: Vec<usize> = files
                .iter()
                .copied()
                .filter(|id| self.jvm.roots.get(id) == Some(own))
                .collect();
            if !mine.is_empty() {
                return mine;
            }
        }
        let directories: HashSet<&[String]> = files
            .iter()
            .map(|id| self.nodes[*id].directory.as_slice())
            .collect();
        if directories.len() == 1 {
            files.clone()
        } else {
            Vec::new()
        }
    }

    /// The one file at the module path `path` that a `language` file can
    /// import, or none where none or several are.
    fn module(&self, path: &[String], language: Language) -> Vec<usize> {
        Found::first([self.modules_at([path.to_vec()], |target| imports_across(language, target))])
    }

    /// What the files at any of `paths`, module paths, of a language
    /// `accepts` takes name.
    fn modules_at(
        &self,
        paths: impl IntoIterator<Item = Vec<String>>,
        accepts: impl Fn(Language) -> bool,
    ) -> Found {
        Found::of(
            paths
                .into_iter()
                .flat_map(|path| {
                    self.exact
                        .by_module
                        .get(path.as_slice())
                        .into_iter()
                        .flatten()
                        .copied()
                        .collect::<Vec<_>>()
                })
                .filter(|id| accepts(self.nodes[*id].language)),
        )
    }

    /// The module an absolute Python import names: the file at that dotted
    /// path below a directory on `sys.path`, searched in the order Python
    /// would put them there — the importer's own (where it runs from as a
    /// script, or where its top-level package is imported from), then the
    /// project root, then the directory above each other top-level package.
    /// Two files on one footing name neither. A path below none of them (the
    /// standard library, an installed distribution) names nothing in the
    /// project, however its last segments match a project file: `logging` is
    /// not `app/util/logging.py`, whose name is `app.util.logging`.
    fn python_module(&self, module: &[String], from: &Node) -> Vec<usize> {
        if module.is_empty() {
            return Vec::new();
        }
        let own = self.python.base_of(&from.directory);
        let tiers = [
            own.into_iter().collect(),
            vec![&[][..]],
            self.python.roots.clone(),
        ];
        let mut searched = HashSet::new();
        Found::first(tiers.into_iter().map(|tier: Vec<&[String]>| {
            let bases: Vec<&[String]> = tier
                .into_iter()
                .filter(|base| searched.insert(*base))
                .collect();
            self.modules_at(
                bases.into_iter().map(|base| [base, module].concat()),
                |language| language == Language::Python,
            )
        }))
    }

    /// The files of the package that is the directory at `path`, in a
    /// language whose packages are directories. A file beside it named like
    /// the directory is not the package. A Go package is the directory's `.go`
    /// files the build reads: neither its tests (`_test.go`, whichever package
    /// they declare) nor the files it ignores (named from `_` or `.`).
    fn package(&self, path: &[String], language: Language) -> Vec<usize> {
        let Some(files) = self.exact.by_directory.get(path) else {
            return Vec::new();
        };
        files
            .iter()
            .copied()
            .filter(|id| {
                let node = &self.nodes[*id];
                let name = Path::new(&node.rel_path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                node.language == language
                    && !(language == Language::Go
                        && (name.ends_with("_test.go") || name.starts_with(['_', '.'])))
            })
            .collect()
    }
}

/// Whether the language defines a package as a directory, so that a
/// reference to one names every file in it and never a single module file.
/// Go's specification says so outright, and a Terraform module's source is
/// its directory; the rest address a directory through a `mod`/`index` file,
/// which `module_path` already folds in.
fn package_is_directory(language: Language) -> bool {
    matches!(language, Language::Go | Language::Terraform)
}

/// Whether the language is one of the JVM's, which share one namespace of
/// packages and classes: a Kotlin file imports a Java class by the name its
/// package declaration gives it, as a Java file does.
fn is_jvm(language: Language) -> bool {
    matches!(
        language,
        Language::Java | Language::Kotlin | Language::Scala
    )
}

/// Whether a `from` file's import can name a `to` file: one of its own
/// language, or any of the JavaScript family, which import each other.
fn imports_across(from: Language, to: Language) -> bool {
    let javascript = |language| {
        matches!(
            language,
            Language::JavaScript | Language::TypeScript | Language::Vue
        )
    };
    from == to || (javascript(from) && javascript(to))
}

/// What a project's build manifests declare about the names its own imports
/// are written against, where those names are not paths on disk. Reading them
/// is how the languages themselves resolve such imports; inferring the names
/// from the tree would be a guess about which components are real.
#[derive(Default)]
struct Manifests {
    /// The module path each `go.mod` declares, by the directory it roots. Go
    /// has no relative import form in a module, so every import of a project
    /// package spells its module's path in full.
    go_modules: HashMap<Vec<String>, Vec<String>>,
    /// The name each `pubspec.yaml` gives the Dart package rooted at its
    /// directory: a `package:name/x.dart` URI is that directory's
    /// `lib/x.dart`.
    dart_packages: HashMap<Vec<String>, String>,
    /// The Cargo package each `Cargo.toml` with a `[package]` roots at its
    /// directory.
    rust_packages: HashMap<Vec<String>, RustPackage>,
}

/// What Cargo builds of a package: its library, where it has one, and the
/// directory of each target's root file, where that crate's module paths
/// start.
#[derive(Debug, Clone, Default)]
struct RustPackage {
    library: Option<RustLibrary>,
    roots: Vec<Vec<String>>,
}

/// A Cargo package's library: the name other crates import it by, and the
/// directory holding its root file.
#[derive(Debug, Clone)]
struct RustLibrary {
    name: String,
    root: Vec<String>,
}

impl Manifests {
    fn read(root: &Path, nodes: &[Node]) -> Self {
        let go_modules = manifests_above(root, nodes, Language::Go, "go.mod")
            .into_iter()
            .filter_map(|(directory, go_mod)| Some((directory, go_module_path(&go_mod)?)))
            .collect();
        let dart_packages = manifests_above(root, nodes, Language::Dart, "pubspec.yaml")
            .into_iter()
            .filter_map(|(directory, pubspec)| Some((directory, dart_package_name(&pubspec)?)))
            .collect();
        let rust_files: HashSet<&Path> = nodes
            .iter()
            .filter(|node| node.language == Language::Rust)
            .map(|node| Path::new(&node.rel_path))
            .collect();
        let rust_packages = manifests_above(root, nodes, Language::Rust, "Cargo.toml")
            .into_iter()
            .filter_map(|(directory, cargo)| {
                let package = cargo_package(&directory, &cargo, &rust_files)?;
                Some((directory, package))
            })
            .collect();
        Self {
            go_modules,
            dart_packages,
            rust_packages,
        }
    }

    /// The directory a `crate::` path written in `from` starts at: that of the
    /// root file of the one target of its package whose directory holds it.
    /// A file under a target directory of its own (`src/bin/`, `tests/`,
    /// `examples/`, `benches/`), or below two targets' roots at once, belongs
    /// to a crate its location does not determine, and so does a file no
    /// `Cargo.toml` governs.
    fn rust_crate_root(&self, from: &Node) -> Option<&[String]> {
        let (directory, package) = nearest(&self.rust_packages, &from.directory)?;
        let target = match &from.directory[directory.len()..] {
            [first, second, ..] if first == "src" && second == "bin" => true,
            [first, ..] => matches!(first.as_str(), "tests" | "examples" | "benches"),
            [] => false,
        };
        if target {
            return None;
        }
        let mut holding = package
            .roots
            .iter()
            .filter(|root| from.directory.starts_with(root));
        match (holding.next(), holding.next()) {
            (Some(root), None) => Some(root),
            _ => None,
        }
    }

    /// The root directory of the project library a Rust path's first segment
    /// names, by the rule every manifest name follows (`own_or_only`).
    fn rust_library(&self, name: &str, from: &Node) -> Option<&[String]> {
        own_or_only(&self.rust_packages, from, |package| {
            package
                .library
                .as_ref()
                .is_some_and(|library| library.name == name)
        })
        .and_then(|(_, package)| package.library.as_ref())
        .map(|library| library.root.as_slice())
    }

    /// The directory a Go import path names in the project, as the go command
    /// finds it. The module providing a package is the one with the longest
    /// path the import path begins with — where two `go.mod` files declare
    /// that path, the one every manifest name means (`own_or_only`) — and the
    /// package is the directory as far below that module's root, unless a
    /// `go.mod` of its own makes that directory another module, which the
    /// first does not contain. An import path under no module the project
    /// declares names a package the importer's own module vendors, or nothing
    /// in the project.
    fn go_package(&self, import: &[String], from: &Node) -> Option<Vec<String>> {
        let longest = self
            .go_modules
            .values()
            .filter(|module| import.starts_with(module))
            .map(Vec::len)
            .max();
        let Some(longest) = longest else {
            let (own, _) = nearest(&self.go_modules, &from.directory)?;
            let mut vendored = own.clone();
            vendored.push("vendor".to_string());
            vendored.extend_from_slice(import);
            return Some(vendored);
        };
        let (root, _) = own_or_only(&self.go_modules, from, |module| {
            module.len() == longest && import.starts_with(module)
        })?;
        let mut directory = root.clone();
        directory.extend_from_slice(&import[longest..]);
        let nested = self
            .go_modules
            .keys()
            .any(|other| other.len() > root.len() && directory.starts_with(other));
        (!nested).then_some(directory)
    }

    /// The directory of the package a `package:name/…` URI written in `from`
    /// names, by the rule every manifest name follows (`own_or_only`).
    fn dart_package(&self, name: &str, from: &Node) -> Option<&[String]> {
        own_or_only(&self.dart_packages, from, |declared| declared == name)
            .map(|(directory, _)| directory.as_slice())
    }
}

/// The manifest nearest above `directory`, with the directory holding it.
fn nearest<'m, T>(
    manifests: &'m HashMap<Vec<String>, T>,
    directory: &[String],
) -> Option<(&'m Vec<String>, &'m T)> {
    (0..=directory.len())
        .rev()
        .find_map(|depth| manifests.get_key_value(&directory[..depth]))
}

/// The one of the manifests declaring a name that an importer means: its own
/// — the nearest above it — where that one `declares` the name, otherwise the
/// only one in the project that does. Where several do and none is the
/// importer's own, which of them it depends on is configuration pack does not
/// read, so the name means none of them.
fn own_or_only<'m, T>(
    manifests: &'m HashMap<Vec<String>, T>,
    from: &Node,
    declares: impl Fn(&T) -> bool,
) -> Option<(&'m Vec<String>, &'m T)> {
    if let Some(own) = nearest(manifests, &from.directory)
        && declares(own.1)
    {
        return Some(own);
    }
    let mut declaring = manifests.iter().filter(|(_, manifest)| declares(manifest));
    match (declaring.next(), declaring.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Each directory holding a `manifest` file on the way from a `language`
/// file up to the project root, with the manifest's contents — every manifest
/// that governs some file of that language, wherever it sits.
fn manifests_above(
    root: &Path,
    nodes: &[Node],
    language: Language,
    manifest: &str,
) -> Vec<(Vec<String>, String)> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for node in nodes.iter().filter(|node| node.language == language) {
        for depth in (0..=node.directory.len()).rev() {
            let directory = &node.directory[..depth];
            // Every directory above one already visited was visited with it.
            if !seen.insert(directory) {
                break;
            }
            let path = directory
                .iter()
                .fold(root.to_path_buf(), |path, part| path.join(part))
                .join(manifest);
            if let Ok(contents) = std::fs::read_to_string(path) {
                found.push((directory.to_vec(), contents));
            }
        }
    }
    found
}

/// The package name a `pubspec.yaml` declares in its top-level `name` key.
fn dart_package_name(pubspec: &str) -> Option<String> {
    pubspec
        .lines()
        .find_map(|line| line.strip_prefix("name:"))
        .map(|name| {
            name.split(" #")
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches(['"', '\''])
                .to_string()
        })
        .filter(|name| !name.is_empty())
}

/// What a `Cargo.toml` declares, where it declares a package: its library —
/// named by `[lib] name` or the package's name with `-` read as `_`, its root
/// file where `[lib] path` (by default `src/lib.rs`) puts it — and the
/// directory of every target root file: the library's, each `[[bin]]` path's,
/// and `src/` for `src/main.rs`. A target counts only where its root file is a
/// project file; a workspace's own manifest declares no package.
fn cargo_package(
    directory: &[String],
    cargo_toml: &str,
    rust_files: &HashSet<&Path>,
) -> Option<RustPackage> {
    let manifest: toml::Table = toml::from_str(cargo_toml).ok()?;
    let package = manifest.get("package")?.as_table()?.get("name")?.as_str()?;
    let project_file = |path: &str| {
        let mut file = directory.to_vec();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                // A root file outside the package's own tree is not followed.
                ".." => return None,
                part => file.push(part.to_string()),
            }
        }
        let present = rust_files.contains(file.iter().collect::<PathBuf>().as_path());
        file.pop();
        present.then_some(file)
    };
    let lib = manifest.get("lib").and_then(toml::Value::as_table);
    let setting = |key| {
        lib.and_then(|lib| lib.get(key))
            .and_then(toml::Value::as_str)
    };
    let library = project_file(setting("path").unwrap_or("src/lib.rs")).map(|root| RustLibrary {
        name: setting("name").unwrap_or(package).replace('-', "_"),
        root,
    });
    let bins = manifest
        .get("bin")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bin| bin.get("path")?.as_str())
        .chain(["src/main.rs"]);
    let mut roots: Vec<Vec<String>> = library
        .iter()
        .map(|library| library.root.clone())
        .chain(bins.filter_map(project_file))
        .collect();
    roots.sort();
    roots.dedup();
    Some(RustPackage { library, roots })
}

/// The module path a `go.mod` declares, read by that file's lexical rules:
/// tokens separated by whitespace, `//` comments to the end of a line, a path
/// that may be written as an interpreted or a raw string, and the directive
/// on one line or as a `module ( … )` block. A path whose string carries an
/// escape is not decoded here, so it declares nothing.
fn go_module_path(go_mod: &str) -> Option<Vec<String>> {
    let mut lines = go_mod.lines().map(go_mod_tokens);
    while let Some(tokens) = lines.next() {
        let path = match tokens.as_slice() {
            [Some(verb), Some(open)] if verb == "module" && open == "(" => lines
                .find(|line| !line.is_empty())
                .and_then(|line| match line.as_slice() {
                    [path] => path.clone(),
                    _ => None,
                }),
            [Some(verb), path] if verb == "module" => path.clone(),
            _ => continue,
        };
        return path
            .map(|path| split_components(&path))
            .filter(|path| !path.is_empty());
    }
    None
}

/// The tokens of one `go.mod` line, `None` standing for a string whose value
/// is not decoded here.
fn go_mod_tokens(line: &str) -> Vec<Option<String>> {
    const PUNCTUATION: [char; 7] = ['(', ')', '[', ']', '{', '}', ','];
    let mut tokens = Vec::new();
    let mut rest = line.trim_start();
    while !rest.is_empty() && !rest.starts_with("//") {
        let first = rest.chars().next().expect("a non-empty line");
        if PUNCTUATION.contains(&first) {
            tokens.push(Some(first.to_string()));
            rest = &rest[1..];
        } else if first == '"' || first == '`' {
            let Some(end) = rest[1..].find(first) else {
                tokens.push(None);
                break;
            };
            let value = &rest[1..1 + end];
            tokens.push((first == '`' || !value.contains('\\')).then(|| value.to_string()));
            rest = &rest[end + 2..];
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || PUNCTUATION.contains(&c))
                .unwrap_or(rest.len());
            let end = rest[..end].find("//").unwrap_or(end);
            tokens.push(Some(rest[..end].to_string()));
            rest = &rest[end..];
        }
        rest = rest.trim_start();
    }
    tokens
}

// --- PageRank --------------------------------------------------------------

fn personalization_vector(nodes: &[Node], focus: Option<&str>) -> BTreeMap<usize, f64> {
    let mut v = BTreeMap::new();
    let n = nodes.len().max(1) as f64;
    let baseline = 1.0 / n;

    if let Some(focus_str) = focus {
        let focus_lower = focus_str.to_lowercase();
        let focused: Vec<usize> = nodes
            .iter()
            .filter(|n| n.rel_path.to_lowercase().contains(&focus_lower))
            .map(|n| n.id)
            .collect();
        let bonus = 0.9 / focused.len() as f64;
        let leftover = 0.1 / n;
        for node in nodes {
            let mut weight = leftover;
            if focused.contains(&node.id) {
                weight += bonus;
            }
            v.insert(node.id, weight);
        }
    } else {
        for node in nodes {
            v.insert(node.id, baseline);
        }
    }
    v
}

fn page_rank(
    graph: &Graph,
    personalization: &BTreeMap<usize, f64>,
    cfg: &PackConfig,
) -> BTreeMap<usize, f64> {
    let n = graph.len();
    if n == 0 {
        return BTreeMap::new();
    }

    let inv_n = 1.0 / n as f64;
    let mut score: BTreeMap<usize, f64> = graph.keys().map(|k| (*k, inv_n)).collect();

    for _ in 0..cfg.max_iterations {
        let mut next: BTreeMap<usize, f64> = graph
            .keys()
            .map(|k| {
                let teleport = personalization.get(k).copied().unwrap_or(inv_n);
                (*k, (1.0 - cfg.damping) * teleport)
            })
            .collect();

        let mut dangling = 0.0;
        for (src, dsts) in graph {
            let s = score[src];
            if dsts.is_empty() {
                dangling += s;
            } else {
                let share = cfg.damping * s / dsts.len() as f64;
                for dst in dsts {
                    *next.entry(*dst).or_insert(0.0) += share;
                }
            }
        }
        let dangling_share = cfg.damping * dangling * inv_n;
        for v in next.values_mut() {
            *v += dangling_share;
        }

        let delta: f64 = score
            .iter()
            .map(|(k, old)| (next.get(k).copied().unwrap_or(0.0) - old).abs())
            .sum();
        score = next;
        if delta < cfg.convergence_tolerance {
            break;
        }
    }
    score
}

// --- budget fit ------------------------------------------------------------

/// The files a budget kept and what they cost. Not a `PackResult`: a pack
/// also has to say what the walk behind it could not read, and this step never
/// saw the walk — returning the whole shape here would mean writing an empty
/// list for the caller to overwrite, which is a claim of completeness one
/// forgotten line turns into a lie.
struct Fitted {
    files: Vec<PackedFile>,
    estimated_tokens: usize,
    unreadable_paths: Vec<PathBuf>,
}

fn fit_to_budget(
    nodes: &[Node],
    ranks: &BTreeMap<usize, f64>,
    budget: usize,
    per_file: usize,
    root: &Path,
    cache: Option<&PackCache>,
) -> Fitted {
    let mut ordered: Vec<&Node> = nodes.iter().collect();
    ordered.sort_by(|a, b| {
        let ra = ranks.get(&a.id).copied().unwrap_or(0.0);
        let rb = ranks.get(&b.id).copied().unwrap_or(0.0);
        // Total order: rank desc, then rel_path asc (unique) as a tiebreak.
        // `sort_by` is stable, so a rank tie would otherwise fall through to the
        // input order; the rel_path tiebreak makes the comparator a self-contained
        // total order (rel_path is unique among nodes), dropping the same file at
        // the budget boundary every run regardless of input arrival order.
        rb.partial_cmp(&ra)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });

    let mut packed = Vec::new();
    let mut spent = 0usize;
    let mut unreadable_paths = Vec::new();
    for node in ordered {
        if estimate_tokens_for_file(&node.rel_path, &[]) >= budget.saturating_sub(spent) {
            continue;
        }
        let signatures = match node_signatures(node, root, cache) {
            Ok(signatures) => signatures,
            Err(error) => {
                if crate::infra::hides_text(&error) {
                    unreadable_paths.push(root.join(&node.rel_path));
                }
                continue;
            }
        };
        if signatures.is_empty() {
            continue;
        }
        let declared = signatures.len();
        let mut shown = Vec::new();
        for signature in signatures.into_iter().take(per_file) {
            shown.push(signature);
            if estimate_tokens_for_file(&node.rel_path, &shown) > budget - spent {
                shown.pop();
                break;
            }
        }
        if shown.is_empty() {
            continue;
        }
        let tokens = estimate_tokens_for_file(&node.rel_path, &shown);
        if spent + tokens > budget {
            continue;
        }
        spent += tokens;
        packed.push(PackedFile {
            path: PathBuf::from(&node.rel_path),
            language: node.language,
            rank: ranks.get(&node.id).copied().unwrap_or(0.0),
            symbols: shown,
            declared_symbols: declared,
        });
        if spent >= budget {
            break;
        }
    }

    Fitted {
        files: packed,
        estimated_tokens: spent,
        unreadable_paths,
    }
}

fn node_signatures(
    node: &Node,
    root: &Path,
    cache: Option<&PackCache>,
) -> std::io::Result<Vec<PackedSymbol>> {
    if let Some(signatures) = &node.signatures {
        return Ok(signatures.clone());
    }
    let path = root.join(&node.rel_path);
    let source = std::fs::read_to_string(&path)?;
    let metadata = std::fs::metadata(&path)?;
    if file_mtime(&metadata) != Some(node.mtime) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "Source changed while building the context pack",
        ));
    }
    let signatures = signatures_from(&source, node.language);
    if let Some(cache) = cache {
        let entry = CachedEntry {
            mtime: node.mtime,
            language: node.language,
            imports: node.imports.clone(),
            signatures: Some(signatures.clone()),
        };
        if let Err(error) = cache.put(&node.rel_path, &entry) {
            tracing::debug!("pack signature cache write failed: {error}");
        }
    }
    Ok(signatures)
}

/// Every signature the file states. The per-file cap belongs to the request,
/// not to the file, so it is applied where the answer is assembled — a cache
/// holding a capped list would answer a later request with an earlier one's
/// shape.
fn signatures_from(source: &str, language: Language) -> Vec<PackedSymbol> {
    crate::services::symbols::SymbolExtractor::shared()
        .extract_signatures(Path::new(""), source, language)
        .into_iter()
        .filter(|(symbol, _)| {
            use crate::models::symbol::SymbolKind;
            !matches!(
                symbol.kind,
                SymbolKind::Field | SymbolKind::EnumMember | SymbolKind::TypeParameter
            ) && (language == Language::Terraform
                || !matches!(symbol.kind, SymbolKind::Variable | SymbolKind::Property))
        })
        .map(|(symbol, signature)| PackedSymbol {
            name: symbol.name,
            kind: symbol.kind.to_string(),
            line: symbol.location.line,
            signature,
        })
        .collect()
}

fn estimate_tokens_for_file(rel_path: &str, symbols: &[PackedSymbol]) -> usize {
    let header = estimate_tokens(rel_path) + 8;
    let body: usize = symbols
        .iter()
        .map(|s| {
            estimate_tokens(&s.name) + estimate_tokens(&s.kind) + estimate_tokens(&s.signature) + 6
        })
        .sum();
    header + body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: usize, rel: &str, contents: &str) -> Node {
        node_in(id, rel, contents, Language::Rust)
    }

    fn node_in(id: usize, rel: &str, contents: &str, language: Language) -> Node {
        Node {
            id,
            rel_path: rel.to_string(),
            language,
            directory: directory_path(Path::new(rel), Path::new("")),
            module_path: module_path(Path::new(rel), Path::new(""), language),
            imports: ImportExtractor::shared().extract(contents, language),
            mtime: 0,
            signatures: Some(signatures_from(contents, language)),
        }
    }

    fn edges_of(nodes: &[Node], id: usize) -> Vec<String> {
        edges_under(nodes, id, &Manifests::default())
    }

    /// Manifests declaring each Go module path at its directory, as the
    /// `go.mod` files there would.
    fn go_modules(modules: &[(&str, &str)]) -> Manifests {
        Manifests {
            go_modules: modules
                .iter()
                .map(|(directory, module)| (split_components(directory), split_components(module)))
                .collect(),
            ..Manifests::default()
        }
    }

    /// Manifests declaring one Cargo package at the project root, its
    /// library `app` rooted in `src/`, as the default layout has it.
    fn default_cargo_layout() -> Manifests {
        let src = vec!["src".to_string()];
        Manifests {
            rust_packages: HashMap::from([(
                Vec::new(),
                RustPackage {
                    library: Some(RustLibrary {
                        name: "app".to_string(),
                        root: src.clone(),
                    }),
                    roots: vec![src],
                },
            )]),
            ..Manifests::default()
        }
    }

    fn rust_edges(nodes: &[Node], id: usize) -> Vec<String> {
        edges_under(nodes, id, &default_cargo_layout())
    }

    /// The manifests a project holding these files declares, read from disk
    /// as a pack over `nodes` would read them.
    fn manifests_of(files: &[(&str, &str)], nodes: &[Node]) -> Manifests {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, contents) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write manifest");
        }
        Manifests::read(dir.path(), nodes)
    }

    fn edges_under(nodes: &[Node], id: usize, manifests: &Manifests) -> Vec<String> {
        build_import_graph(nodes, manifests)
            .edges
            .get(&id)
            .map(|dsts| {
                dsts.iter()
                    .map(|d| nodes[*d].rel_path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_directory_module_still_lives_in_its_own_directory() {
        assert_eq!(
            directory_path(Path::new("/repo/src/cli/mod.rs"), Path::new("/repo")),
            vec!["src", "cli"]
        );
        assert_eq!(
            directory_path(Path::new("/repo/src/cli/output.rs"), Path::new("/repo")),
            vec!["src", "cli"]
        );
        assert!(directory_path(Path::new("/repo/main.rs"), Path::new("/repo")).is_empty());
    }

    #[test]
    fn module_path_drops_the_extension_and_collapses_directory_modules() {
        assert_eq!(
            module_path(
                Path::new("/repo/src/services/pack.rs"),
                Path::new("/repo"),
                Language::Rust
            ),
            vec!["src", "services", "pack"]
        );
        assert_eq!(
            module_path(
                Path::new("/repo/src/services/lsp/mod.rs"),
                Path::new("/repo"),
                Language::Rust
            ),
            vec!["src", "services", "lsp"]
        );
        assert_eq!(
            module_path(
                Path::new("/repo/web/components/index.ts"),
                Path::new("/repo"),
                Language::TypeScript
            ),
            vec!["web", "components"]
        );
    }

    /// A reference is parsed with the anchor it was written against, and a
    /// relative one keeps the hop count that makes it exact.
    #[test]
    fn a_reference_keeps_the_anchor_it_was_written_against() {
        let abs = |c: &[&str]| Reference::Absolute(c.iter().map(|s| s.to_string()).collect());
        let rel = |up, c: &[&str]| Reference::Relative {
            up,
            components: c.iter().map(|s| s.to_string()).collect(),
        };

        assert_eq!(
            Reference::parse("crate::services::pack", Language::Rust),
            Reference::Crate(vec!["services".into(), "pack".into()])
        );
        assert_eq!(
            Reference::parse("serde::Serialize", Language::Rust),
            abs(&["serde", "Serialize"])
        );
        assert_eq!(
            Reference::parse("github.com/acme/w/internal/store", Language::Go),
            abs(&["github.com", "acme", "w", "internal", "store"])
        );
        assert_eq!(
            Reference::parse("./services/pack", Language::TypeScript),
            rel(0, &["services", "pack"])
        );
        assert_eq!(
            Reference::parse("../../shared/util", Language::TypeScript),
            rel(2, &["shared", "util"])
        );
        assert_eq!(
            Reference::parse("super::super::util", Language::Rust),
            rel(2, &["util"])
        );
        assert_eq!(
            Reference::parse("self::helper", Language::Rust),
            rel(0, &["helper"])
        );
        assert_eq!(
            Reference::parse("..pkg.sibling", Language::Python),
            rel(1, &["pkg", "sibling"])
        );
        assert_eq!(Reference::parse(".", Language::Python), rel(0, &[]));
        assert_eq!(Reference::parse("self", Language::Rust), rel(0, &[]));
        assert_eq!(Reference::parse("super", Language::Rust), rel(1, &[]));
        assert_eq!(
            Reference::parse("crate", Language::Rust),
            Reference::Crate(Vec::new())
        );
        assert_eq!(Reference::parse("b.dart", Language::Dart), rel(0, &["b"]));
        assert_eq!(
            Reference::parse("src/../../x.g.dart", Language::Dart),
            rel(1, &["x.g"])
        );
        assert_eq!(
            Reference::parse("package:app/src/a.dart", Language::Dart),
            Reference::Package {
                package: "app".into(),
                components: vec!["src".into(), "a".into()]
            }
        );
        for unresolved in [
            "dart:async",
            "http://x/a.dart",
            "b",
            "a%20b.dart",
            "/abs.dart",
        ] {
            assert_eq!(
                Reference::parse(unresolved, Language::Dart),
                Reference::Unresolved,
                "{unresolved}"
            );
        }
    }

    /// Dart resolves every URI against the library it is written in, so one
    /// without a scheme names the file beside the importer whether or not it
    /// starts with `./` — never a same-named file elsewhere.
    #[test]
    fn a_dart_uri_without_a_scheme_is_relative_to_the_importing_file() {
        let nodes = vec![
            node_in(
                0,
                "lib/src/a.dart",
                "import 'b.dart';\nimport 'sub/../c.dart';\nimport 'd';\n\
                 import 'dart:async';\npart 'a.g.dart';\n",
                Language::Dart,
            ),
            node_in(1, "lib/src/b.dart", "", Language::Dart),
            node_in(2, "test/b.dart", "", Language::Dart),
            node_in(3, "lib/src/a.g.dart", "part of 'a.dart';\n", Language::Dart),
            node_in(4, "lib/src/c.dart", "", Language::Dart),
            node_in(5, "lib/src/d.dart", "", Language::Dart),
        ];
        assert_eq!(
            edges_of(&nodes, 0),
            vec!["lib/src/b.dart", "lib/src/c.dart", "lib/src/a.g.dart"]
        );
        assert_eq!(edges_of(&nodes, 3), vec!["lib/src/a.dart"]);
    }

    /// Two files can share a trailing name and be reached by relative
    /// references of different depth. Dropping the depth made both
    /// references ambiguous and cost the edges entirely.
    #[test]
    fn relative_references_of_different_depth_reach_different_files() {
        let nodes = vec![
            node_in(
                0,
                "src/a/b/caller.ts",
                "import { x } from \"./utils\";\n",
                Language::TypeScript,
            ),
            node_in(1, "src/a/b/utils.ts", "", Language::TypeScript),
            node_in(2, "src/a/utils.ts", "", Language::TypeScript),
            node_in(
                3,
                "src/a/b/other.ts",
                "import { y } from \"../utils\";\n",
                Language::TypeScript,
            ),
        ];
        assert_eq!(edges_of(&nodes, 0), vec!["src/a/b/utils.ts"]);
        assert_eq!(edges_of(&nodes, 3), vec!["src/a/utils.ts"]);
    }

    #[test]
    fn relative_paths_only_reach_exact_project_paths() {
        for (language, caller, import, target, nested) in [
            (
                Language::TypeScript,
                "caller.ts",
                "import {helper} from './helper';\n",
                "helper.ts",
                "nested/helper.ts",
            ),
            (
                Language::Python,
                "pkg/caller.py",
                "from .helper import value\n",
                "pkg/helper.py",
                "nested/pkg/helper.py",
            ),
            (
                Language::Terraform,
                "main.tf",
                "module \"network\" { source = \"./network\" }\n",
                "network/main.tf",
                "nested/network/main.tf",
            ),
        ] {
            let mut nodes = vec![
                node_in(0, caller, import, language),
                node_in(1, nested, "", language),
            ];
            assert!(edges_of(&nodes, 0).is_empty(), "{language:?}");
            nodes.push(node_in(2, target, "", language));
            assert_eq!(edges_of(&nodes, 0), vec![target], "{language:?}");
        }
    }

    /// Go repeats its declared module path in every intra-project import,
    /// and that prefix is not a directory anywhere in the tree. Without
    /// reading it out of `go.mod` a Go project forms no edges at all, and
    /// the ranking it feeds degenerates to uniform.
    #[test]
    fn a_go_import_resolves_through_the_module_path_go_mod_declares() {
        let nodes = vec![
            node_in(
                0,
                "cmd/server/main.go",
                "import \"github.com/acme/widget/internal/store\"\n",
                Language::Go,
            ),
            node_in(1, "internal/store/store.go", "", Language::Go),
            node_in(2, "internal/store/query.go", "", Language::Go),
        ];
        // A Go package is a directory, so the reference names every file in it.
        assert_eq!(
            edges_under(&nodes, 0, &go_modules(&[("", "github.com/acme/widget")])),
            vec!["internal/store/store.go", "internal/store/query.go"]
        );
        // Without the declared prefix the reference matches nothing.
        assert!(edges_of(&nodes, 0).is_empty());
    }

    /// A Go import path always names a package directory — the build's files
    /// in it — and never a file beside it that happens to share its name, nor
    /// the package's tests. One under no module the project declares (the
    /// standard library, a dependency) names nothing here, however its last
    /// segments match a project path.
    #[test]
    fn a_go_import_names_a_package_directory_of_a_declared_module_only() {
        let nodes = vec![
            node_in(
                0,
                "cmd/app/main.go",
                "package main\nimport (\n\t\"example.com/svc/internal/config\"\n\t\
                 \"example.com/svc/internal/store\"\n\t\"log\"\n)\n",
                Language::Go,
            ),
            node_in(1, "cmd/app/log.go", "package main\n", Language::Go),
            node_in(2, "internal/config.go", "package internal\n", Language::Go),
            node_in(
                3,
                "internal/config/load.go",
                "package config\n",
                Language::Go,
            ),
            node_in(4, "internal/context.go", "package internal\n", Language::Go),
            node_in(
                5,
                "internal/store/store.go",
                "package store\nimport (\n\t\"context\"\n\t\"errors\"\n\t\"log\"\n\t\
                 \"github.com/other/svc/internal/config\"\n)\n",
                Language::Go,
            ),
            node_in(
                6,
                "internal/store/errors.go",
                "package store\n",
                Language::Go,
            ),
            node_in(
                7,
                "internal/store/store_test.go",
                "package store_test\n",
                Language::Go,
            ),
            node_in(
                8,
                "internal/store/_scratch.go",
                "package store\n",
                Language::Go,
            ),
        ];
        let manifests = go_modules(&[("", "example.com/svc")]);
        assert_eq!(
            edges_under(&nodes, 0, &manifests),
            vec![
                "internal/config/load.go",
                "internal/store/store.go",
                "internal/store/errors.go"
            ]
        );
        assert!(edges_under(&nodes, 5, &manifests).is_empty());
    }

    /// Every `go.mod` roots its own module, wherever it sits: a workspace's
    /// modules import each other by their declared paths, a directory with a
    /// `go.mod` of its own is no package of the module above it, and a path
    /// under no declared module names what the importer's module vendors.
    #[test]
    fn every_go_mod_maps_its_module_path_to_its_own_directory() {
        let util = "import \"example.com/backend/pkg/util\"\n";
        let nodes = vec![
            node_in(
                0,
                "backend/cmd/main.go",
                &format!("{util}import \"example.com/backend/plugins/auth\"\n"),
                Language::Go,
            ),
            node_in(1, "backend/pkg/util/u.go", "", Language::Go),
            node_in(2, "backend/plugins/auth/auth.go", "", Language::Go),
            node_in(
                3,
                "tools/gen/g.go",
                &format!("{util}import \"github.com/dep/lib\"\n"),
                Language::Go,
            ),
            node_in(
                4,
                "tools/vendor/github.com/dep/lib/lib.go",
                "",
                Language::Go,
            ),
            node_in(
                5,
                "backend/vendor/github.com/dep/lib/lib.go",
                "",
                Language::Go,
            ),
        ];
        let manifests = manifests_of(
            &[
                ("backend/go.mod", "module example.com/backend\n"),
                ("tools/go.mod", "module example.com/tools\n"),
                ("backend/plugins/go.mod", "module example.com/plugins\n"),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 0, &manifests),
            vec!["backend/pkg/util/u.go"]
        );
        assert_eq!(
            edges_under(&nodes, 3, &manifests),
            vec![
                "backend/pkg/util/u.go",
                "tools/vendor/github.com/dep/lib/lib.go"
            ]
        );
    }

    /// Two `go.mod` files can declare one module path — a copied example, a
    /// fork. A file of either imports its own module's packages; any other
    /// importer cannot tell which of them it builds against.
    #[test]
    fn a_go_module_path_two_go_mods_declare_names_only_the_importers_own() {
        let import = "package main\nimport \"example.com/app/lib\"\n";
        let nodes = vec![
            node_in(0, "a/cmd/main.go", import, Language::Go),
            node_in(1, "a/lib/lib.go", "package lib\n", Language::Go),
            node_in(2, "b/cmd/main.go", import, Language::Go),
            node_in(3, "b/lib/lib.go", "package lib\n", Language::Go),
            node_in(4, "c/main.go", import, Language::Go),
        ];
        let manifests = go_modules(&[
            ("a", "example.com/app"),
            ("b", "example.com/app"),
            ("c", "example.com/c"),
        ]);
        assert_eq!(edges_under(&nodes, 0, &manifests), vec!["a/lib/lib.go"]);
        assert_eq!(edges_under(&nodes, 2, &manifests), vec!["b/lib/lib.go"]);
        assert!(edges_under(&nodes, 4, &manifests).is_empty());
    }

    /// `go.mod` is read by its own lexical rules, not as a line beginning
    /// `module `: the path may be quoted or followed by a comment, the
    /// directive may be a block, and a commented-out directive is no
    /// directive.
    #[test]
    fn a_go_mod_module_directive_is_read_as_go_mod_spells_it() {
        for go_mod in [
            "module github.com/acme/widget\n\ngo 1.24\n",
            "module github.com/acme/widget // the widget\n",
            "module\tgithub.com/acme/widget\n",
            "module \"github.com/acme/widget\"\n",
            "module `github.com/acme/widget`\n",
            "// module example.com/fake\nmodule (\n\t// the widget\n\tgithub.com/acme/widget\n)\n",
            "  module github.com/acme/widget//vanity\r\n",
        ] {
            assert_eq!(
                go_module_path(go_mod),
                Some(split_components("github.com/acme/widget")),
                "{go_mod:?}"
            );
        }
        for go_mod in [
            "module \"github.com/acme/wi\\x64get\"\n",
            "// module example.com/fake\n",
            "module\n",
            "modules example.com/fake\n",
        ] {
            assert_eq!(go_module_path(go_mod), None, "{go_mod:?}");
        }
    }

    /// A Dart file imports its own package's files as `package:name/…`, which
    /// names the `lib` directory of the package whose `pubspec.yaml` declares
    /// that name, wherever in the project it sits.
    #[test]
    fn a_dart_package_uri_resolves_under_the_lib_of_the_package_its_pubspec_names() {
        let nodes = vec![
            node_in(
                0,
                "lib/main.dart",
                "import 'package:app/src/a.dart';\nimport 'package:core/model.dart';\n\
                 import 'package:flutter/material.dart';\n",
                Language::Dart,
            ),
            node_in(1, "lib/src/a.dart", "", Language::Dart),
            node_in(2, "packages/core/lib/model.dart", "", Language::Dart),
            node_in(3, "packages/core/lib/src/a.dart", "", Language::Dart),
        ];
        let manifests = manifests_of(
            &[
                ("pubspec.yaml", "name: app # the package\nversion: 1.0.0\n"),
                ("packages/core/pubspec.yaml", "name: core\n"),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 0, &manifests),
            vec!["lib/src/a.dart", "packages/core/lib/model.dart"]
        );
        assert_eq!(
            dart_package_name("  name: nested\nname: \"quoted\"\n"),
            Some("quoted".into())
        );
    }

    /// Two pubspecs can declare one name — a fork, a copied example. The
    /// importer's own package answers to its name whatever else declares it;
    /// any other importer cannot tell which of them it depends on, so the URI
    /// names neither, whichever manifest was read last.
    #[test]
    fn a_dart_package_name_two_pubspecs_declare_names_only_the_importers_own() {
        let import = "import 'package:core/src/a.dart';\n";
        let nodes = vec![
            node_in(0, "app/lib/main.dart", import, Language::Dart),
            node_in(1, "legacy/core/lib/core.dart", import, Language::Dart),
            node_in(2, "legacy/core/lib/src/a.dart", "", Language::Dart),
            node_in(3, "packages/core/lib/core.dart", import, Language::Dart),
            node_in(4, "packages/core/lib/src/a.dart", "", Language::Dart),
        ];
        let manifests = manifests_of(
            &[
                ("app/pubspec.yaml", "name: app\n"),
                ("packages/core/pubspec.yaml", "name: core\n"),
                ("legacy/core/pubspec.yaml", "name: core\n"),
            ],
            &nodes,
        );
        assert!(edges_under(&nodes, 0, &manifests).is_empty());
        assert_eq!(
            edges_under(&nodes, 1, &manifests),
            vec!["legacy/core/lib/src/a.dart"]
        );
        assert_eq!(
            edges_under(&nodes, 3, &manifests),
            vec!["packages/core/lib/src/a.dart"]
        );
    }

    /// A JVM import names classes and packages by what files declare, not by
    /// where they sit: a class is its file's declared package and name, and a
    /// whole package is the files declaring it in the importer's own source
    /// root — main code sees none of the test sources sharing its packages.
    /// Java, Kotlin and Scala share that one namespace.
    #[test]
    fn a_jvm_import_names_what_files_declare_in_the_importers_source_root() {
        let java = |id, rel, source| node_in(id, rel, source, Language::Java);
        let kotlin = |id, rel, source| node_in(id, rel, source, Language::Kotlin);
        let util = "package com.acme.util;\n";
        let nodes = vec![
            java(
                0,
                "src/main/java/com/acme/app/App.java",
                "package com.acme.app;\nimport com.acme.util.*;\nimport java.util.List;\n\
                 import com.acme.model.User;\n",
            ),
            java(1, "src/main/java/com/acme/util/Strings.java", util),
            java(2, "src/main/java/com/acme/util/Numbers.java", util),
            java(3, "src/test/java/com/acme/util/StringsTest.java", util),
            java(4, "src/test/java/com/acme/util/Numbers.java", util),
            java(5, "src/main/java/util/List.java", "package util;\n"),
            kotlin(
                6,
                "src/main/kotlin/com/acme/kt/Main.kt",
                "package com.acme.kt\nimport com.acme.util.Strings\n\
                 import com.acme.util.Numbers\nimport com.acme.model.User.Companion.create\n",
            ),
            // Kotlin does not tie a file's directory to its package.
            kotlin(
                7,
                "src/main/kotlin/model/User.kt",
                "package com.acme.model\n",
            ),
            java(
                8,
                "src/test/java/com/acme/app/AppTest.java",
                "package com.acme.app;\nimport com.acme.util.*;\n",
            ),
            java(9, "src/main/java/com/acme/util/Undeclared.java", ""),
        ];
        assert_eq!(
            edges_of(&nodes, 0),
            vec![
                "src/main/java/com/acme/util/Strings.java",
                "src/main/java/com/acme/util/Numbers.java",
                "src/main/kotlin/model/User.kt"
            ]
        );
        // `Numbers` is declared in two source roots, neither the importer's.
        assert_eq!(
            edges_of(&nodes, 6),
            vec![
                "src/main/java/com/acme/util/Strings.java",
                "src/main/kotlin/model/User.kt"
            ]
        );
        assert_eq!(
            edges_of(&nodes, 8),
            vec![
                "src/test/java/com/acme/util/StringsTest.java",
                "src/test/java/com/acme/util/Numbers.java"
            ]
        );
    }

    #[test]
    fn a_crate_path_resolves_to_the_file_of_its_module_below_the_crate_root() {
        let nodes = vec![
            node(0, "src/cli/output.rs", "use crate::services::pack;\n"),
            node(1, "src/services/pack.rs", ""),
            node(2, "src/cli/services/pack.rs", ""),
        ];
        assert_eq!(rust_edges(&nodes, 0), vec!["src/services/pack.rs"]);
        // No `Cargo.toml` governs the file, so nothing says where its crate's
        // root is.
        assert!(edges_of(&nodes, 0).is_empty());
    }

    #[test]
    fn a_reference_that_names_an_item_resolves_to_its_module() {
        let nodes = vec![
            node(
                0,
                "src/cli/output.rs",
                "use crate::services::pack::PackConfig;\n",
            ),
            node(1, "src/services/pack.rs", ""),
        ];
        assert_eq!(rust_edges(&nodes, 0), vec!["src/services/pack.rs"]);
    }

    /// The failure a bare-name index cannot avoid: an external crate whose
    /// segments happen to match a project file's name. `std::process` and
    /// `serde::Serialize` name nothing in this project, so they contribute
    /// no edge, while the project's own library is found by its name.
    #[test]
    fn an_external_reference_never_lands_on_a_same_named_project_file() {
        let nodes = vec![
            node(
                0,
                "src/cli/output.rs",
                "use std::process::Command;\nuse serde::Serialize;\nuse log::info;\n",
            ),
            node(1, "src/services/dist/process.rs", ""),
            node(2, "src/util/serde.rs", ""),
            node(3, "src/log.rs", ""),
            node(4, "tests/cli.rs", "use app::services::pack;\n"),
            node(5, "src/services/pack.rs", ""),
        ];
        assert!(rust_edges(&nodes, 0).is_empty());
        assert_eq!(rust_edges(&nodes, 4), vec!["src/services/pack.rs"]);
    }

    /// An absolute Python import names a module below a directory on
    /// `sys.path`: `import logging` is the standard library's, not
    /// `app/util/logging.py`, whose name is `app.util.logging`, and a script
    /// finds the modules beside it where another directory's script does not.
    #[test]
    fn an_absolute_python_import_names_a_module_on_sys_path_only() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            python(0, "app/__init__.py", ""),
            python(
                1,
                "app/main.py",
                "import logging\nimport util.logging\nimport app.util.logging\n\
                 from app.util import helpers\n",
            ),
            python(2, "app/util/__init__.py", ""),
            python(3, "app/util/logging.py", ""),
            python(4, "src/pkg/__init__.py", ""),
            python(5, "src/pkg/core.py", ""),
            python(
                6,
                "tests/test_core.py",
                "import pkg.core\nimport conftest\n",
            ),
            python(7, "tests/conftest.py", ""),
            python(8, "scripts/run.py", "import config\n"),
            python(9, "scripts/config.py", ""),
            python(10, "tools/lint.py", "import config\nimport core\n"),
            python(11, "a/app/__init__.py", ""),
            python(12, "a/app/db.py", ""),
            python(13, "b/app/__init__.py", ""),
            python(14, "b/app/db.py", ""),
            python(15, "a/app/serve.py", "import app.db\n"),
            python(16, "c/serve.py", "import app.db\n"),
        ];
        assert_eq!(
            edges_of(&nodes, 1),
            vec!["app/util/logging.py", "app/util/__init__.py"]
        );
        assert_eq!(
            edges_of(&nodes, 6),
            vec!["src/pkg/core.py", "tests/conftest.py"]
        );
        assert_eq!(edges_of(&nodes, 8), vec!["scripts/config.py"]);
        assert!(edges_of(&nodes, 10).is_empty());
        // An importer inside a top-level package finds that package first;
        // one outside every `app` cannot tell which of them it means.
        assert_eq!(edges_of(&nodes, 15), vec!["a/app/db.py"]);
        assert!(edges_of(&nodes, 16).is_empty());
    }

    /// A name that is no path in the project — a JavaScript package
    /// specifier, a Terraform registry source — names what the project's
    /// dependencies provide, however a project file's path ends.
    #[test]
    fn a_package_specifier_never_lands_on_a_project_file_it_spells() {
        let nodes = vec![
            node_in(
                0,
                "src/app.ts",
                "import React from \"react\";\nimport merge from \"lodash/merge\";\n\
                 import { util } from \"./util\";\n",
                Language::TypeScript,
            ),
            node_in(1, "src/react.ts", "", Language::TypeScript),
            node_in(2, "src/lodash/merge.ts", "", Language::TypeScript),
            node_in(3, "src/util.ts", "", Language::TypeScript),
            node_in(
                4,
                "main.tf",
                "module \"vpc\" { source = \"terraform-aws-modules/vpc/aws\" }\n",
                Language::Terraform,
            ),
            node_in(5, "modules/vpc/aws/main.tf", "", Language::Terraform),
        ];
        assert_eq!(edges_of(&nodes, 0), vec!["src/util.ts"]);
        assert!(edges_of(&nodes, 4).is_empty());
    }

    /// Four files can share a basename; a reference short enough to fit all
    /// of them identifies none, and inventing an edge to each would hand
    /// every one of them the same borrowed rank.
    #[test]
    fn an_ambiguous_reference_yields_no_edge() {
        let nodes = vec![
            node(0, "src/cli/output.rs", "use symbols;\n"),
            node(1, "src/services/store/symbols.rs", ""),
            node(2, "src/services/lsp/symbols.rs", ""),
        ];
        assert!(rust_edges(&nodes, 0).is_empty());

        let qualified = vec![
            node(
                0,
                "src/cli/output.rs",
                "use crate::services::store::symbols;\n",
            ),
            node(1, "src/services/store/symbols.rs", ""),
            node(2, "src/services/lsp/symbols.rs", ""),
        ];
        assert_eq!(
            rust_edges(&qualified, 0),
            vec!["src/services/store/symbols.rs"]
        );
    }

    /// A path from a name is relative to the module writing it only where the
    /// file declares that name as a module; any other names an extern crate,
    /// which no project file stands for, so nothing is left out of the graph
    /// and nothing is disclosed.
    #[test]
    fn only_a_path_through_a_declared_module_needs_module_ownership() {
        for (source, omitted) in [
            ("use anyhow::Result;\nuse std::fmt;\n", false),
            (
                "mod config { pub struct Settings; }\nuse config::Settings;\n",
                true,
            ),
            ("mod config;\n", true),
        ] {
            let nodes = vec![node(0, "src/main.rs", source)];
            let graph = build_import_graph(&nodes, &default_cargo_layout());
            assert_eq!(graph.rust_relative_imports_omitted, omitted, "{source}");
        }
    }

    /// A directory module is addressed by its directory, not by `mod`.
    #[test]
    fn a_directory_module_resolves_through_its_directory_name() {
        let nodes = vec![
            node(0, "src/app.rs", "use crate::services::lsp::LspService;\n"),
            node(1, "src/services/lsp/mod.rs", ""),
        ];
        assert_eq!(rust_edges(&nodes, 0), vec!["src/services/lsp/mod.rs"]);
    }

    #[test]
    fn rust_relative_imports_require_module_ownership() {
        for (file, source, child, decoy) in [
            (
                "src/foo.rs",
                "mod child; use self::child as dependency;",
                "src/foo/child.rs",
                "src/child.rs",
            ),
            (
                "src/custom_root.rs",
                "mod child; use self::child as dependency;",
                "src/child.rs",
                "src/custom_root/child.rs",
            ),
            (
                "src/handlers/mod.rs",
                "mod child;",
                "src/handlers/child.rs",
                "src/child.rs",
            ),
            (
                "src/lib.rs",
                "mod child;",
                "src/child.rs",
                "src/lib/child.rs",
            ),
            (
                "src/lib.rs",
                "mod outer { mod child; use self::child as dependency; use super::other; }",
                "src/outer/child.rs",
                "src/child.rs",
            ),
            (
                "src/lib.rs",
                "mod outer { use self as current; use super as parent; }",
                "src/self.rs",
                "src/super.rs",
            ),
        ] {
            let nodes = vec![
                node(
                    0,
                    file,
                    &format!("{source}\nuse crate as root;\nuse crate::known;\n"),
                ),
                node(1, child, ""),
                node(2, decoy, ""),
                node(3, "src/known.rs", ""),
                node(4, "src/crate.rs", ""),
            ];
            let graph = build_import_graph(&nodes, &default_cargo_layout());
            assert!(graph.rust_relative_imports_omitted, "{file}");
            assert_eq!(graph.edges[&0], vec![3], "{file}");
        }
    }

    /// A `crate::` path starts at the root of the crate writing it, which
    /// Cargo's layout determines for a package's library and `src/main.rs`
    /// but not for a target directory of its own; a workspace member's
    /// library is imported by the name its `Cargo.toml` gives it.
    #[test]
    fn a_rust_path_starts_where_cargo_roots_its_crate() {
        let nodes = vec![
            node(0, "crates/core/src/lib.rs", ""),
            node(1, "crates/core/src/model.rs", ""),
            node(
                2,
                "crates/cli/src/main.rs",
                "use acme_core::model::User;\nuse cli::args;\nuse crate::run;\n",
            ),
            node(3, "crates/cli/src/run.rs", ""),
            node(4, "crates/cli/lib/root.rs", ""),
            node(5, "crates/cli/lib/args.rs", "use crate::flags;\n"),
            node(6, "crates/cli/lib/flags.rs", ""),
            node(7, "crates/cli/src/bin/tool.rs", "use crate::run;\n"),
            node(8, "crates/core/tests/it.rs", "use crate::model;\n"),
            node(9, "crates/cli/src/tools/gen.rs", ""),
            node(10, "crates/cli/src/tools/emit.rs", "use crate::run;\n"),
        ];
        let manifests = manifests_of(
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
                (
                    "crates/core/Cargo.toml",
                    "[package]\nname = \"acme-core\"\n",
                ),
                (
                    "crates/cli/Cargo.toml",
                    "[package]\nname = \"acme-cli\"\n[lib]\nname = \"cli\"\npath = \"lib/root.rs\"\n\
                     [[bin]]\nname = \"gen\"\npath = \"src/tools/gen.rs\"\n",
                ),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 2, &manifests),
            vec![
                "crates/core/src/model.rs",
                "crates/cli/lib/args.rs",
                "crates/cli/src/run.rs"
            ]
        );
        assert_eq!(
            edges_under(&nodes, 5, &manifests),
            vec!["crates/cli/lib/flags.rs"]
        );
        // A binary of `src/bin/` and an integration test are crates of their
        // own, whose `crate::` paths are omitted and disclosed.
        assert!(edges_under(&nodes, 7, &manifests).is_empty());
        assert!(edges_under(&nodes, 8, &manifests).is_empty());
        // Below both `src/` and the `gen` binary's root, a file could be a
        // module of either.
        assert!(edges_under(&nodes, 10, &manifests).is_empty());
        let rooted = build_import_graph(&nodes[..7], &manifests);
        assert!(!rooted.rust_relative_imports_omitted);
        assert!(build_import_graph(&nodes, &manifests).rust_relative_imports_omitted);
    }

    #[test]
    fn signatures_exclude_bodies_across_grammars() {
        let cases = [
            (
                Language::Rust,
                "pub fn work() { println!(\"BODY_MARKER\"); }",
            ),
            (
                Language::Python,
                "@decorate\ndef work():\n    print('BODY_MARKER')\n",
            ),
            (
                Language::Go,
                "package main\nfunc work() { println(\"BODY_MARKER\") }",
            ),
            (
                Language::TypeScript,
                "export const work = () => { console.log('BODY_MARKER'); };",
            ),
            (
                Language::JavaScript,
                "export function work() { console.log('BODY_MARKER'); }",
            ),
            (
                Language::Java,
                "class Worker { void work() { System.out.println(\"BODY_MARKER\"); } }",
            ),
            (Language::Kotlin, "fun work() { println(\"BODY_MARKER\") }"),
            (Language::Cpp, "void work() { puts(\"BODY_MARKER\"); }"),
            (
                Language::CSharp,
                "class Worker { void work() { Console.WriteLine(\"BODY_MARKER\"); } }",
            ),
            (
                Language::PHP,
                "<?php function work() { echo 'BODY_MARKER'; }",
            ),
            (Language::Ruby, "def work\n puts 'BODY_MARKER'\nend"),
            (Language::Bash, "work() { echo BODY_MARKER; }"),
            (Language::Lua, "function work() print('BODY_MARKER') end"),
            (Language::Swift, "func work() { print(\"BODY_MARKER\") }"),
            (
                Language::Scala,
                "def work(): Unit = { println(\"BODY_MARKER\") }",
            ),
            (Language::Dart, "void work() { print('BODY_MARKER'); }"),
            (
                Language::Terraform,
                "resource \"null_resource\" \"work\" { triggers = { marker = \"BODY_MARKER\" } }",
            ),
        ];
        for (language, source) in cases {
            let signatures = signatures_from(source, language);
            assert!(
                !signatures.is_empty(),
                "{language:?} produced no signatures"
            );
            assert!(
                signatures
                    .iter()
                    .all(|symbol| !symbol.signature.contains("BODY_MARKER")),
                "{language:?}: {signatures:?}"
            );
        }
        let go = signatures_from(
            "package p\ntype Point struct { X int }\ntype ID string",
            Language::Go,
        );
        assert!(
            go.iter()
                .any(|symbol| symbol.signature == "type Point struct"),
            "{go:?}"
        );
        assert!(
            go.iter().any(|symbol| symbol.signature == "type ID string"),
            "{go:?}"
        );
        let terraform = signatures_from(
            "variable \"region\" { default = \"BODY_MARKER\" }",
            Language::Terraform,
        );
        assert_eq!(terraform.len(), 1);
        assert!(!terraform[0].signature.contains("BODY_MARKER"));
        let python = signatures_from("@decorate\ndef work():\n    pass", Language::Python);
        assert_eq!(python[0].signature, "@decorate\ndef work()");
    }

    #[test]
    fn signatures_follow_declarations_and_preserve_multiline_headers() {
        let source = "\"\"\"\ndef fabricated():\n    pass\n\"\"\"\n\ndef real(\n    value: str,\n) -> str:\n    return value\n";
        let signatures = signatures_from(source, Language::Python);
        assert_eq!(signatures.len(), 1);
        assert_eq!(signatures[0].name, "real");
        assert!(signatures[0].signature.contains("value: str"));
        assert!(signatures[0].signature.ends_with(") -> str"));
        assert!(!signatures[0].signature.contains("return value"));
    }

    #[test]
    fn pagerank_converges_on_simple_chain() {
        let mut g: Graph = BTreeMap::new();
        g.insert(0, vec![1]);
        g.insert(1, vec![2]);
        g.insert(2, vec![]);
        let pers: BTreeMap<usize, f64> = (0..3).map(|i| (i, 1.0 / 3.0)).collect();
        let ranks = page_rank(&g, &pers, &PackConfig::default());
        let total: f64 = ranks.values().sum();
        assert!(
            (total - 1.0).abs() < 1e-3,
            "ranks must sum to ~1, got {total}"
        );
        let max_id = ranks
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(*max_id, 2);
    }

    #[test]
    fn personalization_biases_toward_focus() {
        let nodes = vec![
            node(0, "src/auth.rs", ""),
            node(1, "src/render.rs", ""),
            node(2, "src/util.rs", ""),
        ];
        let pers = personalization_vector(&nodes, Some("auth"));
        assert!(pers[&0] > pers[&1]);
        assert!(pers[&0] > pers[&2]);
    }

    #[test]
    fn personalization_uniform_when_focus_missing() {
        let nodes = vec![node(0, "src/auth.rs", ""), node(1, "src/render.rs", "")];
        let pers = personalization_vector(&nodes, None);
        assert!((pers[&0] - pers[&1]).abs() < 1e-9);
    }

    #[test]
    fn unmatched_focus_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auth.rs"), "fn auth() {}\n").unwrap();
        let result = build_pack(
            dir.path(),
            1000,
            Some("nonexistent"),
            &FileFilter::new(dir.path()),
            &PackConfig::default(),
        );
        assert!(matches!(result, Err(PackError::UnmatchedFocus(_))));
    }

    #[test]
    fn fit_to_budget_respects_token_cap() {
        let big_signature = format!("pub fn f() {{ {} }}", "x".repeat(2000));
        let nodes = vec![
            node(
                0,
                "src/a.rs",
                &format!("{big_signature}\n{big_signature}\n{big_signature}\n"),
            ),
            node(1, "src/b.rs", "pub fn small() {}\n"),
        ];
        let ranks: BTreeMap<usize, f64> = [(0, 0.6), (1, 0.4)].into();
        let result = fit_to_budget(
            &nodes,
            &ranks,
            50,
            PACK_SYMBOLS_PER_FILE,
            Path::new(""),
            None,
        );
        assert!(result.estimated_tokens <= 50);
        let tiny = fit_to_budget(
            &nodes,
            &ranks,
            1,
            PACK_SYMBOLS_PER_FILE,
            Path::new(""),
            None,
        );
        assert!(tiny.files.is_empty());
        assert_eq!(tiny.estimated_tokens, 0);
    }

    /// The cap belongs to the request. A cache holding a list already cut to
    /// one request's shape answers the next one with it, which is what left
    /// `--per-file` working only on a cold cache.
    #[test]
    fn the_per_file_cap_is_applied_to_the_answer_not_stored_in_the_cache() {
        let source = "pub fn one() {}\npub fn two() {}\npub fn three() {}\n";
        let signatures = signatures_from(source, Language::Rust);
        assert_eq!(
            signatures.len(),
            3,
            "extraction states what the file has: {signatures:?}"
        );

        let nodes = vec![Node {
            id: 0,
            rel_path: "a.rs".to_string(),
            directory: vec![],
            module_path: vec!["a".to_string()],
            language: Language::Rust,
            imports: FileImports::default(),
            mtime: 0,
            signatures: Some(signatures),
        }];
        let ranks = BTreeMap::from([(0usize, 1.0)]);
        for cap in [1usize, 2, 3, 9] {
            let fitted = fit_to_budget(&nodes, &ranks, 10_000, cap, Path::new(""), None);
            let file = &fitted.files[0];
            assert_eq!(file.symbols.len(), cap.min(3), "cap {cap}");
            assert_eq!(file.declared_symbols, 3, "the file states three either way");
        }
    }

    #[test]
    fn fit_to_budget_orders_by_rank() {
        let nodes = vec![
            node(0, "src/low.rs", "pub fn low() {}\n"),
            node(1, "src/high.rs", "pub fn high() {}\n"),
        ];
        let ranks: BTreeMap<usize, f64> = [(0, 0.1), (1, 0.9)].into();
        let result = fit_to_budget(
            &nodes,
            &ranks,
            10_000,
            PACK_SYMBOLS_PER_FILE,
            Path::new(""),
            None,
        );
        assert_eq!(result.files[0].path, PathBuf::from("src/high.rs"));
        assert_eq!(result.files[1].path, PathBuf::from("src/low.rs"));
    }

    #[test]
    fn fit_to_budget_tie_breaks_by_rel_path() {
        // Equal rank, both fit: the order is decided by rel_path ascending,
        // independent of the input Vec order — so a genuine tie is reproducible
        // rather than left to the unstable sort's arrival order. Input is given
        // in reversed (zebra-before-alpha) order on purpose.
        let nodes = vec![
            node(0, "src/zebra.rs", "pub fn z() {}\n"),
            node(1, "src/alpha.rs", "pub fn a() {}\n"),
        ];
        let ranks: BTreeMap<usize, f64> = [(0, 0.5), (1, 0.5)].into();
        let result = fit_to_budget(
            &nodes,
            &ranks,
            10_000,
            PACK_SYMBOLS_PER_FILE,
            Path::new(""),
            None,
        );
        assert_eq!(result.files[0].path, PathBuf::from("src/alpha.rs"));
        assert_eq!(result.files[1].path, PathBuf::from("src/zebra.rs"));
    }

    #[test]
    fn import_graph_links_use_to_target() {
        let a = node(0, "src/a.rs", "use crate::b;\npub fn a() {}\n");
        let b = node(1, "src/b.rs", "pub fn b() {}\n");
        let graph = build_import_graph(&[a, b], &default_cargo_layout());
        assert!(graph.edges[&0].contains(&1));
        assert!(graph.edges[&1].is_empty());
        assert!(!graph.rust_relative_imports_omitted);
    }
    #[test]
    fn a_small_pack_defers_signatures_until_the_file_can_be_selected() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(
                dir.path().join(format!("{name}.rs")),
                format!("fn {name}() {{}}\n"),
            )
            .unwrap();
        }
        let one = signatures_from("fn a() {}\n", Language::Rust);
        let budget = estimate_tokens_for_file("a.rs", &one);
        let cfg = PackConfig::default();
        let result =
            build_pack(dir.path(), budget, None, &FileFilter::new(dir.path()), &cfg).unwrap();
        assert_eq!(result.files.len(), 1);
        let cache = PackCache::open(dir.path()).unwrap();
        assert!(cache.get("a.rs").unwrap().signatures.is_some());
        assert!(cache.get("b.rs").unwrap().signatures.is_none());
        assert!(cache.get("c.rs").unwrap().signatures.is_none());
        drop(cache);
        let larger =
            build_pack(dir.path(), 10000, None, &FileFilter::new(dir.path()), &cfg).unwrap();
        assert_eq!(larger.files.len(), 3);
    }
    #[test]
    fn relative_imports_preserve_python_depth_and_literal_file_stems() {
        let python = vec![
            node_in(
                0,
                "pkg/a/b/caller.py",
                "from ...shared import value\n",
                Language::Python,
            ),
            node_in(1, "pkg/shared.py", "", Language::Python),
            node_in(2, "pkg/a/shared.py", "", Language::Python),
        ];
        assert_eq!(edges_of(&python, 0), vec!["pkg/shared.py"]);
        let js = vec![
            node_in(
                0,
                "web/caller.ts",
                "import {value} from './util.test.js';\n",
                Language::TypeScript,
            ),
            node_in(1, "web/util.test.ts", "", Language::TypeScript),
            node_in(2, "web/util.test.py", "", Language::Python),
        ];
        assert_eq!(edges_of(&js, 0), vec!["web/util.test.ts"]);
        let escaped = vec![
            node_in(
                0,
                "web/caller.ts",
                "import {helper} from './he\\u006cper';\n",
                Language::TypeScript,
            ),
            node_in(1, "web/helper.ts", "", Language::TypeScript),
            node_in(2, "web/he\\u006cper.ts", "", Language::TypeScript),
        ];
        assert!(edges_of(&escaped, 0).is_empty());
        for (literal, target, decoy) in [
            ("./helper'", "web/helper'.ts", "web/helper.ts"),
            ("./helper`", "web/helper`.ts", "web/helper.ts"),
            ("./ helper ", "web/ helper .ts", "web/ helper.ts"),
        ] {
            let nodes = vec![
                node_in(
                    0,
                    "web/caller.ts",
                    &format!("import \"{literal}\";\n"),
                    Language::TypeScript,
                ),
                node_in(1, target, "", Language::TypeScript),
                node_in(2, decoy, "", Language::TypeScript),
            ];
            assert_eq!(edges_of(&nodes, 0), vec![target], "{literal:?}");
        }
        let tf = vec![
            node_in(
                0,
                "main.tf",
                "module \"network\" { source = \"./network\" }\n",
                Language::Terraform,
            ),
            node_in(1, "network/main.tf", "", Language::Terraform),
            node_in(2, "network/variables.tf", "", Language::Terraform),
            // A module's source is its directory, never a file named like it.
            node_in(3, "network.tf", "", Language::Terraform),
        ];
        assert_eq!(
            edges_of(&tf, 0),
            vec!["network/main.tf", "network/variables.tf"]
        );
        for (literal, target, decoy) in [
            ("./he\\u006cper", "helper/main.tf", "he\\u006cper/main.tf"),
            ("./$${helper}", "${helper}/main.tf", "$${helper}/main.tf"),
            ("./%%{helper}", "%{helper}/main.tf", "%%{helper}/main.tf"),
        ] {
            let nodes = vec![
                node_in(
                    0,
                    "main.tf",
                    &format!("module \"helper\" {{ source = \"{literal}\" }}\n"),
                    Language::Terraform,
                ),
                node_in(1, target, "", Language::Terraform),
                node_in(2, decoy, "", Language::Terraform),
            ];
            assert!(edges_of(&nodes, 0).is_empty(), "{literal:?}");
        }
    }
}
