//! Token-budgeted context pack — PageRank-ranked file selection with
//! import-graph edges, then signature-only excerpts truncated to fit a
//! caller-supplied token budget.
//!
//! This is the engine behind `symora pack`. It is intentionally
//! self-contained: it does not depend on the LSP, the daemon, or a built
//! search index. A single repo walk + per-language regex extraction is
//! enough to produce a useful, deterministic pack.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;

use crate::constants::defaults::{PACK_MAX_FILE_BYTES, PACK_SYMBOLS_PER_FILE};
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::Language;
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
            max_file_size_bytes: PACK_MAX_FILE_BYTES,
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
) -> Result<PackResult> {
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

    let nodes = collect_nodes(root, file_filter, cfg, cache.as_ref());

    if let Some(cache) = cache.as_ref() {
        let active: HashSet<String> = nodes.iter().map(|n| n.rel_path.clone()).collect();
        if let Err(e) = cache.prune(&active) {
            tracing::debug!("pack cache prune failed: {e}");
        }
    }

    let graph = build_import_graph(&nodes, &declared_module_prefix(root));
    let personalization = personalization_vector(&nodes, focus);
    let ranks = page_rank(&graph, &personalization, cfg);
    Ok(fit_to_budget(&nodes, &ranks, budget_tokens))
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
    imports: Vec<String>,
    signatures: Vec<PackedSymbol>,
}

fn collect_nodes(
    root: &Path,
    file_filter: &FileFilter,
    cfg: &PackConfig,
    cache: Option<&PackCache>,
) -> Vec<Node> {
    let mut nodes = Vec::new();
    for abs_path in file_filter.discover_files(&[]) {
        let abs_path = abs_path.as_path();
        let language = Language::from_path(abs_path);
        if !is_indexable(language) {
            continue;
        }
        let metadata = match std::fs::metadata(abs_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() > cfg.max_file_size_bytes {
            continue;
        }
        let mtime = file_mtime(&metadata);
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
                imports: cached.imports,
                signatures: cached.signatures,
            });
            continue;
        }

        // Cache miss: pay the I/O + extraction cost, then upsert.
        let contents = match std::fs::read_to_string(abs_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let imports = extract_imports(&contents, language);
        let signatures = top_signatures_from(&contents, language, cfg);

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
    nodes
}

fn file_mtime(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn is_indexable(language: Language) -> bool {
    !matches!(
        language,
        Language::Unknown | Language::Yaml | Language::Toml | Language::Markdown
    )
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

fn build_import_graph(nodes: &[Node], module_prefix: &[String]) -> Graph {
    let mut by_module: HashMap<&[String], Vec<usize>> = HashMap::new();
    let mut by_directory: HashMap<&[String], Vec<usize>> = HashMap::new();
    for node in nodes {
        for start in 0..node.module_path.len() {
            by_module
                .entry(&node.module_path[start..])
                .or_default()
                .push(node.id);
        }
        for start in 0..=node.directory.len() {
            by_directory
                .entry(&node.directory[start..])
                .or_default()
                .push(node.id);
        }
    }
    let index = ProjectIndex {
        by_module,
        by_directory,
        module_prefix,
    };

    let mut graph: Graph = BTreeMap::new();
    for node in nodes {
        graph.entry(node.id).or_default();
        let mut seen = HashSet::new();
        for target in &node.imports {
            for dst in index.resolve(&Reference::parse(target), node) {
                if dst != node.id && seen.insert(dst) {
                    graph.entry(node.id).or_default().push(dst);
                }
            }
        }
    }
    graph
}

/// Where a reference is anchored.
///
/// A path is only meaningful against the thing it is written relative to,
/// and the two anchors carry different information: `../util` says "one
/// directory up from here", which the importing file's own location
/// resolves exactly, while `crate::cli::output` says "from the project's
/// root". Collapsing them — dropping the leading hops and matching whatever
/// tail is left — loses the very part that made the relative form
/// unambiguous, and in a tree with more than one `utils` it loses the edge
/// entirely.
#[derive(Debug, PartialEq, Eq)]
enum Reference {
    /// Anchored at the importing file's directory, `up` levels above it.
    Relative { up: usize, components: Vec<String> },
    /// Anchored at the project root.
    Absolute(Vec<String>),
}

impl Reference {
    fn parse(raw: &str) -> Self {
        let raw = raw
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '`');
        let mut rest = raw;
        let mut up = 0usize;
        let mut relative = false;

        // `./x` and `../x` in the quoted-path languages, `.x` and `..x` in
        // Python, `self::x` and `super::x` in Rust: the same two ideas
        // spelled three ways.
        loop {
            if let Some(tail) = rest.strip_prefix("../").or_else(|| rest.strip_prefix("..")) {
                relative = true;
                up += 1;
                rest = tail;
            } else if let Some(tail) = rest
                .strip_prefix("./")
                .or_else(|| rest.strip_prefix("super::"))
            {
                if rest.starts_with("super::") {
                    up += 1;
                }
                relative = true;
                rest = tail;
            } else if let Some(tail) = rest.strip_prefix("self::") {
                relative = true;
                rest = tail;
            } else if rest.starts_with('.') && !rest.starts_with("..") {
                relative = true;
                rest = &rest[1..];
            } else {
                break;
            }
        }

        let components = split_components(rest);
        if relative {
            Self::Relative { up, components }
        } else {
            Self::Absolute(components)
        }
    }
}

fn split_components(raw: &str) -> Vec<String> {
    raw.split(['/', '.', ':'])
        .filter(|part| !part.is_empty() && *part != "crate")
        .map(str::to_string)
        .collect()
}

/// The project's files, indexed by every suffix of their module path and of
/// their directory, plus the module prefix its own imports carry.
struct ProjectIndex<'a> {
    by_module: HashMap<&'a [String], Vec<usize>>,
    by_directory: HashMap<&'a [String], Vec<usize>>,
    module_prefix: &'a [String],
}

impl ProjectIndex<'_> {
    /// The nodes a reference names, empty when it names something outside
    /// the project or nothing unambiguous.
    fn resolve(&self, reference: &Reference, from: &Node) -> Vec<usize> {
        match reference {
            Reference::Relative { up, components } => {
                let base = from.directory.len().checked_sub(*up);
                let Some(base) = base else {
                    return Vec::new();
                };
                let mut path = from.directory[..base].to_vec();
                path.extend_from_slice(components);
                self.at(&path, from.language)
            }
            Reference::Absolute(components) => {
                // A declared module prefix is part of the reference but not
                // part of the tree, so it is removed before matching rather
                // than hunted for on disk.
                let components = components
                    .strip_prefix(self.module_prefix)
                    .unwrap_or(components);
                // `use a::b::Item` names a module and an item in one path;
                // read it whole, then without the item.
                for candidate in [components, components.split_last().map_or(&[][..], |s| s.1)] {
                    if candidate.is_empty() {
                        continue;
                    }
                    let hit = self.at(candidate, from.language);
                    if !hit.is_empty() {
                        return hit;
                    }
                }
                Vec::new()
            }
        }
    }

    /// The nodes at an exact project path: the file it names, or — where a
    /// package IS a directory — every file the directory holds.
    fn at(&self, path: &[String], language: Language) -> Vec<usize> {
        if let Some(files) = self.by_module.get(path) {
            return match files.as_slice() {
                // A path several files share names none of them.
                [only] => vec![*only],
                _ => Vec::new(),
            };
        }
        if package_is_directory(language)
            && let Some(files) = self.by_directory.get(path)
        {
            return files.clone();
        }
        Vec::new()
    }
}

/// Whether the language defines a package as a directory, so that a
/// reference to one names every file in it rather than a single module
/// file. Go's specification says so outright; the rest address a directory
/// through a `mod`/`index` file, which `module_path` already folds in.
fn package_is_directory(language: Language) -> bool {
    matches!(language, Language::Go)
}

/// The module path a project's own imports are written against, when the
/// language's build manifest declares one that does not exist on disk.
///
/// Go names its module in `go.mod`, and every intra-project import repeats
/// that name in full because Go has no relative import form. Reading it is
/// how the language itself resolves those imports; inferring it from the
/// tree would be a guess about which leading components are real
/// directories.
fn declared_module_prefix(root: &Path) -> Vec<String> {
    let Ok(go_mod) = std::fs::read_to_string(root.join("go.mod")) else {
        return Vec::new();
    };
    go_mod
        .lines()
        .find_map(|line| line.trim().strip_prefix("module "))
        .map(|path| split_components(path.trim()))
        .unwrap_or_default()
}

/// Pull import paths out of a source file using deliberately conservative,
/// deterministic textual rules. A missed reference only costs an edge, and
/// a reference that names something outside the project is discarded by
/// resolution rather than needing to be recognised here.
fn extract_imports(source: &str, language: Language) -> Vec<String> {
    let mut out = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        match language {
            Language::Rust => {
                if let Some(rest) = trimmed
                    .strip_prefix("use ")
                    .or_else(|| trimmed.strip_prefix("pub use "))
                    && let Some(path) = rest.split([';', '{', ' ']).next()
                {
                    out.push(path.trim().to_string());
                } else if let Some(rest) = trimmed.strip_prefix("mod ")
                    && let Some(name) = rest.split([';', '{', ' ']).next()
                {
                    // A `mod` declaration names a sibling file or a
                    // subdirectory of the declaring one — the same anchor
                    // `self::` spells, and the reason it must not be matched
                    // against the whole project.
                    out.push(format!("self::{}", name.trim()));
                }
            }
            Language::Python => {
                if let Some(rest) = trimmed.strip_prefix("from ")
                    && let Some(path) = rest.split_whitespace().next()
                {
                    out.push(path.to_string());
                } else if let Some(rest) = trimmed.strip_prefix("import ")
                    && let Some(path) = rest.split([',', ' ']).next()
                {
                    out.push(path.trim().to_string());
                }
            }
            Language::JavaScript | Language::TypeScript => {
                if let Some(quoted) = quoted_after("from ", trimmed) {
                    out.push(quoted);
                } else if let Some(quoted) = quoted_after("import ", trimmed) {
                    out.push(quoted);
                } else if let Some(quoted) = quoted_after("require(", trimmed) {
                    out.push(quoted);
                }
            }
            Language::Go => {
                if let Some(quoted) = quoted_after("import ", trimmed) {
                    out.push(quoted);
                } else if let Some(quoted) = quoted_segment(trimmed) {
                    // Inside `import ( "a/b"  "c/d" )` blocks
                    out.push(quoted);
                }
            }
            Language::Java | Language::Kotlin | Language::Scala => {
                if let Some(rest) = trimmed.strip_prefix("import ")
                    && let Some(path) = rest.split([';', ' ', '{']).next()
                {
                    out.push(path.trim_start_matches("static ").trim().to_string());
                } else if let Some(rest) = trimmed.strip_prefix("package ")
                    && let Some(path) = rest.split([';', ' ']).next()
                {
                    out.push(path.trim().to_string());
                }
            }
            Language::Swift => {
                if let Some(rest) = trimmed.strip_prefix("import ")
                    && let Some(path) = rest.split_whitespace().next()
                {
                    out.push(path.to_string());
                }
            }
            Language::Elixir => {
                for prefix in ["alias ", "import ", "require ", "use "] {
                    if let Some(rest) = trimmed.strip_prefix(prefix)
                        && let Some(path) = rest.split([',', ' ', '{']).next()
                    {
                        out.push(path.trim().to_string());
                        break;
                    }
                }
            }
            Language::Dart => {
                if let Some(quoted) = quoted_after("import ", trimmed) {
                    out.push(quoted);
                } else if let Some(quoted) = quoted_after("part ", trimmed) {
                    out.push(quoted);
                } else if let Some(quoted) = quoted_after("export ", trimmed) {
                    out.push(quoted);
                }
            }
            Language::Terraform => {
                // Terraform's `module` block points at a source path.
                if trimmed.starts_with("source ")
                    && let Some(quoted) = quoted_segment(trimmed)
                {
                    out.push(quoted);
                }
            }
            _ => {}
        }
    }
    out
}

fn quoted_after(prefix: &str, line: &str) -> Option<String> {
    let idx = line.find(prefix)?;
    let after = &line[idx + prefix.len()..];
    quoted_segment(after)
}

fn quoted_segment(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let quote = bytes
        .iter()
        .position(|b| *b == b'"' || *b == b'\'' || *b == b'`')?;
    let q = bytes[quote];
    let rest = &s[quote + 1..];
    let end = rest.as_bytes().iter().position(|b| *b == q)?;
    Some(rest[..end].to_string())
}

// --- PageRank --------------------------------------------------------------

fn personalization_vector(nodes: &[Node], focus: Option<&str>) -> BTreeMap<usize, f64> {
    let mut v = BTreeMap::new();
    let n = nodes.len().max(1) as f64;
    let baseline = 1.0 / n;

    if let Some(focus_str) = focus {
        let focus_lower = focus_str.to_lowercase();
        let mut focused: Vec<usize> = nodes
            .iter()
            .filter(|n| n.rel_path.to_lowercase().contains(&focus_lower))
            .map(|n| n.id)
            .collect();
        if focused.is_empty() {
            focused = nodes.iter().map(|n| n.id).collect();
        }
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

fn fit_to_budget(nodes: &[Node], ranks: &BTreeMap<usize, f64>, budget: usize) -> PackResult {
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
    for node in ordered {
        if node.signatures.is_empty() {
            continue;
        }
        let tokens = estimate_tokens_for_file(&node.rel_path, &node.signatures);
        if spent + tokens > budget && !packed.is_empty() {
            break;
        }
        spent += tokens;
        packed.push(PackedFile {
            path: PathBuf::from(&node.rel_path),
            language: node.language,
            rank: ranks.get(&node.id).copied().unwrap_or(0.0),
            symbols: node.signatures.clone(),
        });
        if spent >= budget {
            break;
        }
    }

    PackResult {
        files: packed,
        estimated_tokens: spent,
        graph_size: nodes.len(),
    }
}

fn top_signatures_from(source: &str, language: Language, cfg: &PackConfig) -> Vec<PackedSymbol> {
    let mut out = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        if let Some(symbol) = extract_signature(line, language) {
            out.push(PackedSymbol {
                line: (idx + 1) as u32,
                ..symbol
            });
            if out.len() >= cfg.max_symbols_per_file {
                break;
            }
        }
    }
    out
}

/// Extract a top-level signature from a single line. Returns `None` when
/// the line is not a recognized declaration. Conservative on purpose:
/// false negatives only cost packing breadth, not correctness.
fn extract_signature(line: &str, language: Language) -> Option<PackedSymbol> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
        return None;
    }
    let signature = trimmed.trim_end_matches('{').trim_end().to_string();

    match language {
        Language::Rust => match_rust(trimmed, signature),
        Language::Python => match_python(trimmed, signature),
        Language::JavaScript | Language::TypeScript => match_js_ts(trimmed, signature),
        Language::Go => match_go(trimmed, signature),
        Language::Java | Language::Kotlin | Language::CSharp => match_jvm(trimmed, signature),
        Language::Swift => match_swift(trimmed, signature),
        Language::Scala => match_scala(trimmed, signature),
        Language::Elixir => match_elixir(trimmed, signature),
        Language::Dart => match_dart(trimmed, signature),
        _ => None,
    }
}

fn match_swift(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    let candidates: &[(&str, &str)] = &[
        ("public class ", "class"),
        ("public struct ", "struct"),
        ("public protocol ", "interface"),
        ("public enum ", "enum"),
        ("public func ", "function"),
        ("class ", "class"),
        ("struct ", "struct"),
        ("protocol ", "interface"),
        ("enum ", "enum"),
        ("func ", "function"),
        ("extension ", "extension"),
    ];
    for (prefix, kind) in candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = first_ident(rest)?;
            return Some(PackedSymbol {
                name,
                kind: (*kind).to_string(),
                line: 0,
                signature,
            });
        }
    }
    None
}

fn match_scala(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    let candidates: &[(&str, &str)] = &[
        ("class ", "class"),
        ("trait ", "interface"),
        ("object ", "object"),
        ("def ", "function"),
        ("type ", "type"),
        ("val ", "constant"),
        ("var ", "variable"),
    ];
    for (prefix, kind) in candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = first_ident(rest)?;
            return Some(PackedSymbol {
                name,
                kind: (*kind).to_string(),
                line: 0,
                signature: signature.trim_end_matches('=').trim_end().to_string(),
            });
        }
    }
    None
}

fn match_elixir(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    let candidates: &[(&str, &str)] = &[
        ("defmodule ", "module"),
        ("defprotocol ", "interface"),
        ("defimpl ", "implementation"),
        ("defstruct ", "struct"),
        ("defmacro ", "macro"),
        ("defmacrop ", "macro"),
        ("defp ", "function"),
        ("def ", "function"),
    ];
    for (prefix, kind) in candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            // Elixir module names: `Foo.Bar` — accept dots in identifier.
            let name = first_dotted_ident(rest)?;
            return Some(PackedSymbol {
                name,
                kind: (*kind).to_string(),
                line: 0,
                signature: signature.trim_end_matches(',').trim_end().to_string(),
            });
        }
    }
    None
}

fn match_dart(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    let candidates: &[(&str, &str)] = &[
        ("abstract class ", "class"),
        ("class ", "class"),
        ("mixin ", "mixin"),
        ("extension ", "extension"),
        ("enum ", "enum"),
        ("typedef ", "type"),
    ];
    for (prefix, kind) in candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = first_ident(rest)?;
            return Some(PackedSymbol {
                name,
                kind: (*kind).to_string(),
                line: 0,
                signature,
            });
        }
    }
    // Dart top-level functions look like `T name(...) {` — match
    // identifier-followed-by-parens with no leading keyword.
    if trimmed.contains('(') && !trimmed.starts_with('@') {
        let head = trimmed.split('(').next().unwrap_or("");
        if let Some(name) = head.split_whitespace().last()
            && name.chars().next().is_some_and(|c| c.is_alphabetic())
        {
            return Some(PackedSymbol {
                name: name
                    .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                    .to_string(),
                kind: "function".to_string(),
                line: 0,
                signature,
            });
        }
    }
    None
}

fn first_dotted_ident(s: &str) -> Option<String> {
    let mut name = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' || c == '.' {
            name.push(c);
        } else if !name.is_empty() {
            break;
        } else if c.is_whitespace() {
            continue;
        } else {
            return None;
        }
    }
    if name.is_empty() { None } else { Some(name) }
}

fn match_rust(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    let candidates: &[(&str, &str)] = &[
        ("pub fn ", "function"),
        ("pub async fn ", "function"),
        ("pub(crate) fn ", "function"),
        ("pub struct ", "struct"),
        ("pub enum ", "enum"),
        ("pub trait ", "interface"),
        ("pub mod ", "module"),
        ("pub type ", "type"),
        ("fn ", "function"),
        ("async fn ", "function"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("trait ", "interface"),
    ];
    for (prefix, kind) in candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = first_ident(rest)?;
            return Some(PackedSymbol {
                name,
                kind: (*kind).to_string(),
                line: 0,
                signature,
            });
        }
    }
    None
}

fn match_python(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    let candidates: &[(&str, &str)] = &[
        ("def ", "function"),
        ("async def ", "function"),
        ("class ", "class"),
    ];
    for (prefix, kind) in candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = first_ident(rest)?;
            return Some(PackedSymbol {
                name,
                kind: (*kind).to_string(),
                line: 0,
                signature: signature.trim_end_matches(':').trim_end().to_string(),
            });
        }
    }
    None
}

fn match_js_ts(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    let candidates: &[(&str, &str)] = &[
        ("export function ", "function"),
        ("export async function ", "function"),
        ("export class ", "class"),
        ("export interface ", "interface"),
        ("export type ", "type"),
        ("export const ", "constant"),
        ("function ", "function"),
        ("async function ", "function"),
        ("class ", "class"),
        ("interface ", "interface"),
    ];
    for (prefix, kind) in candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = first_ident(rest)?;
            return Some(PackedSymbol {
                name,
                kind: (*kind).to_string(),
                line: 0,
                signature,
            });
        }
    }
    None
}

fn match_go(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    if let Some(rest) = trimmed.strip_prefix("func ") {
        // Method: `func (r *Recv) Name(...)` or function `func Name(...)`
        let name_start = if rest.starts_with('(') {
            rest.find(')')? + 1
        } else {
            0
        };
        let name = first_ident(rest[name_start..].trim_start())?;
        return Some(PackedSymbol {
            name,
            kind: "function".to_string(),
            line: 0,
            signature,
        });
    }
    if let Some(rest) = trimmed.strip_prefix("type ") {
        let name = first_ident(rest)?;
        return Some(PackedSymbol {
            name,
            kind: "type".to_string(),
            line: 0,
            signature,
        });
    }
    None
}

fn match_jvm(trimmed: &str, signature: String) -> Option<PackedSymbol> {
    let candidates: &[(&str, &str)] = &[
        ("public class ", "class"),
        ("public interface ", "interface"),
        ("public final class ", "class"),
        ("public abstract class ", "class"),
        ("class ", "class"),
        ("interface ", "interface"),
        ("public fun ", "function"),
        ("fun ", "function"),
    ];
    for (prefix, kind) in candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = first_ident(rest)?;
            return Some(PackedSymbol {
                name,
                kind: (*kind).to_string(),
                line: 0,
                signature,
            });
        }
    }
    None
}

fn first_ident(s: &str) -> Option<String> {
    let mut name = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            name.push(c);
        } else if !name.is_empty() {
            break;
        } else if c.is_whitespace() {
            continue;
        } else {
            return None;
        }
    }
    if name.is_empty() { None } else { Some(name) }
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
        let cfg = PackConfig::default();
        Node {
            id,
            rel_path: rel.to_string(),
            language,
            directory: directory_path(Path::new(rel), Path::new("")),
            module_path: module_path(Path::new(rel), Path::new(""), language),
            imports: extract_imports(contents, language),
            signatures: top_signatures_from(contents, language, &cfg),
        }
    }

    fn edges_of(nodes: &[Node], id: usize) -> Vec<String> {
        edges_with(nodes, id, &[])
    }

    fn edges_with(nodes: &[Node], id: usize, prefix: &[String]) -> Vec<String> {
        build_import_graph(nodes, prefix)
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
            Reference::parse("crate::services::pack"),
            abs(&["services", "pack"])
        );
        assert_eq!(
            Reference::parse("github.com/acme/w/internal/store"),
            abs(&["github", "com", "acme", "w", "internal", "store"])
        );
        assert_eq!(
            Reference::parse("\"./services/pack\""),
            rel(0, &["services", "pack"])
        );
        assert_eq!(
            Reference::parse("\"../../shared/util\""),
            rel(2, &["shared", "util"])
        );
        assert_eq!(Reference::parse("super::super::util"), rel(2, &["util"]));
        assert_eq!(Reference::parse("self::helper"), rel(0, &["helper"]));
        assert_eq!(
            Reference::parse("..pkg.sibling"),
            rel(1, &["pkg", "sibling"])
        );
        assert_eq!(Reference::parse("."), rel(0, &[]));
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

    /// Go repeats its declared module path in every intra-project import,
    /// and that prefix is not a directory anywhere in the tree. Without
    /// reading it out of `go.mod` a Go project forms no edges at all, and
    /// the ranking it feeds degenerates to uniform.
    #[test]
    fn a_go_import_resolves_through_the_module_path_go_mod_declares() {
        let prefix: Vec<String> = ["github", "com", "acme", "widget"]
            .iter()
            .map(|s| s.to_string())
            .collect();
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
            edges_with(&nodes, 0, &prefix),
            vec!["internal/store/store.go", "internal/store/query.go"]
        );
        // Without the declared prefix the reference matches nothing.
        assert!(edges_of(&nodes, 0).is_empty());
    }

    #[test]
    fn a_go_mod_module_line_yields_the_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("go.mod"),
            "module github.com/acme/widget\n\ngo 1.24\n",
        )
        .expect("write go.mod");
        assert_eq!(
            declared_module_prefix(dir.path()),
            vec!["github", "com", "acme", "widget"]
        );
        assert!(declared_module_prefix(Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn a_reference_resolves_to_the_file_whose_module_path_it_ends() {
        let nodes = vec![
            node(0, "src/cli/output.rs", "use crate::services::pack;\n"),
            node(1, "src/services/pack.rs", ""),
        ];
        assert_eq!(edges_of(&nodes, 0), vec!["src/services/pack.rs"]);
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
        assert_eq!(edges_of(&nodes, 0), vec!["src/services/pack.rs"]);
    }

    /// The failure a bare-name index cannot avoid: an external crate whose
    /// final segment happens to match a project file's name. `std::process`
    /// names nothing in this project, so it must contribute no edge.
    #[test]
    fn an_external_reference_never_lands_on_a_same_named_project_file() {
        let nodes = vec![
            node(0, "src/cli/output.rs", "use std::process::Command;\n"),
            node(1, "src/services/dist/process.rs", ""),
        ];
        assert!(edges_of(&nodes, 0).is_empty());
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
        assert!(edges_of(&nodes, 0).is_empty());

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
            edges_of(&qualified, 0),
            vec!["src/services/store/symbols.rs"]
        );
    }

    /// A directory module is addressed by its directory, not by `mod`.
    #[test]
    fn a_directory_module_resolves_through_its_directory_name() {
        let nodes = vec![
            node(0, "src/app.rs", "use crate::services::lsp::LspService;\n"),
            node(1, "src/services/lsp/mod.rs", ""),
        ];
        assert_eq!(edges_of(&nodes, 0), vec!["src/services/lsp/mod.rs"]);
    }

    #[test]
    fn extract_rust_use_paths() {
        let src = "use crate::services::pack;\npub use crate::cli::output;\nmod auth;\n";
        let imports = extract_imports(src, Language::Rust);
        assert!(imports.iter().any(|s| s.contains("services::pack")));
        assert!(imports.iter().any(|s| s.contains("cli::output")));
        assert!(imports.iter().any(|s| s == "self::auth"));
    }

    /// `mod x;` names the declaring file's own neighbour. Resolved against
    /// the whole project it would land on any lone file called `x`, however
    /// unrelated.
    #[test]
    fn a_mod_declaration_names_a_neighbour_not_a_namesake() {
        let nodes = vec![
            node(0, "src/handlers/mod.rs", "mod auth;\n"),
            node(1, "src/handlers/auth.rs", ""),
            node(2, "src/services/auth.rs", ""),
        ];
        assert_eq!(edges_of(&nodes, 0), vec!["src/handlers/auth.rs"]);
    }

    #[test]
    fn extract_python_imports() {
        let src = "from foo.bar import baz\nimport util\n";
        let imports = extract_imports(src, Language::Python);
        assert!(imports.iter().any(|s| s == "foo.bar"));
        assert!(imports.iter().any(|s| s == "util"));
    }

    #[test]
    fn extract_typescript_imports() {
        let src = "import { Foo } from './bar';\nimport baz from \"../baz\";\n";
        let imports = extract_imports(src, Language::TypeScript);
        assert!(imports.iter().any(|s| s.contains("bar")));
        assert!(imports.iter().any(|s| s.contains("baz")));
    }

    #[test]
    fn rust_signature_extraction() {
        let sym = extract_signature("pub fn process(&self) -> Result<()> {", Language::Rust)
            .expect("matched");
        assert_eq!(sym.name, "process");
        assert_eq!(sym.kind, "function");
        assert!(sym.signature.contains("process"));
    }

    #[test]
    fn python_signature_extraction() {
        let sym = extract_signature("def process(self, name):", Language::Python).expect("matched");
        assert_eq!(sym.name, "process");
        assert_eq!(sym.kind, "function");
        assert!(!sym.signature.ends_with(':'));
    }

    #[test]
    fn go_method_signature_extraction() {
        let sym = extract_signature("func (r *Recv) Process(ctx Context) error {", Language::Go)
            .expect("matched");
        assert_eq!(sym.name, "Process");
    }

    #[test]
    fn swift_signature_extraction() {
        let sym =
            extract_signature("public func process() async throws {", Language::Swift).unwrap();
        assert_eq!(sym.name, "process");
        assert_eq!(sym.kind, "function");
    }

    #[test]
    fn scala_signature_extraction() {
        let sym = extract_signature("def process(x: Int): String =", Language::Scala).unwrap();
        assert_eq!(sym.name, "process");
        assert!(!sym.signature.ends_with('='));
    }

    #[test]
    fn elixir_module_signature_extraction() {
        let sym = extract_signature("defmodule My.Module do", Language::Elixir).unwrap();
        assert_eq!(sym.name, "My.Module");
        assert_eq!(sym.kind, "module");
    }

    #[test]
    fn dart_class_signature_extraction() {
        let sym =
            extract_signature("class MyWidget extends StatelessWidget {", Language::Dart).unwrap();
        assert_eq!(sym.name, "MyWidget");
        assert_eq!(sym.kind, "class");
    }

    #[test]
    fn dart_function_signature_extraction() {
        let sym = extract_signature("Future<void> main() async {", Language::Dart).unwrap();
        assert_eq!(sym.name, "main");
        assert_eq!(sym.kind, "function");
    }

    #[test]
    fn extract_swift_imports_test() {
        let imports = extract_imports("import Foundation\nimport SwiftUI\n", Language::Swift);
        assert!(imports.iter().any(|s| s == "Foundation"));
        assert!(imports.iter().any(|s| s == "SwiftUI"));
    }

    #[test]
    fn extract_elixir_imports_test() {
        let imports = extract_imports(
            "alias My.Module\nimport Ecto.Query\nuse GenServer\n",
            Language::Elixir,
        );
        assert!(imports.iter().any(|s| s == "My.Module"));
        assert!(imports.iter().any(|s| s == "Ecto.Query"));
        assert!(imports.iter().any(|s| s == "GenServer"));
    }

    #[test]
    fn extract_dart_imports_test() {
        let imports = extract_imports(
            "import 'package:flutter/material.dart';\nexport 'src/foo.dart';\n",
            Language::Dart,
        );
        assert!(imports.iter().any(|s| s.contains("material")));
        assert!(imports.iter().any(|s| s.contains("foo")));
    }

    #[test]
    fn extract_terraform_module_source_test() {
        let src = "module \"vpc\" {\n  source = \"./modules/vpc\"\n}";
        let imports = extract_imports(src, Language::Terraform);
        assert!(imports.iter().any(|s| s.contains("modules/vpc")));
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
    fn personalization_falls_back_to_uniform_on_no_match() {
        let nodes = vec![node(0, "src/auth.rs", ""), node(1, "src/render.rs", "")];
        let pers = personalization_vector(&nodes, Some("nonexistent"));
        assert!((pers[&0] - pers[&1]).abs() < 1e-3);
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
        let result = fit_to_budget(&nodes, &ranks, 50);
        // We always emit at least one file, but the second should be dropped
        // because the first already overflows the tiny 50-token budget.
        assert_eq!(result.files.len(), 1);
        assert!(result.estimated_tokens > 0);
    }

    #[test]
    fn fit_to_budget_orders_by_rank() {
        let nodes = vec![
            node(0, "src/low.rs", "pub fn low() {}\n"),
            node(1, "src/high.rs", "pub fn high() {}\n"),
        ];
        let ranks: BTreeMap<usize, f64> = [(0, 0.1), (1, 0.9)].into();
        let result = fit_to_budget(&nodes, &ranks, 10_000);
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
        let result = fit_to_budget(&nodes, &ranks, 10_000);
        assert_eq!(result.files[0].path, PathBuf::from("src/alpha.rs"));
        assert_eq!(result.files[1].path, PathBuf::from("src/zebra.rs"));
    }

    #[test]
    fn import_graph_links_use_to_target() {
        let a = node(0, "src/a.rs", "use crate::b;\npub fn a() {}\n");
        let b = node(1, "src/b.rs", "pub fn b() {}\n");
        let graph = build_import_graph(&[a, b], &[]);
        assert!(graph[&0].contains(&1));
        assert!(graph[&1].is_empty());
    }
}
