//! Context packs ranked from syntax-derived module paths.
//!
//! Imports are cached per file. Signatures are loaded in rank order and
//! admitted whole within the caller's approximate token budget.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

mod graph;
mod manifests;

use self::graph::{Graph, build_import_graph};
use self::manifests::Manifests;
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
    /// The specifiers the project's own configuration declares — a TypeScript
    /// `paths` alias, a workspace package's name — that named no file it
    /// determines, so their edges are missing rather than absent.
    pub undetermined_imports: Vec<String>,
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
            undetermined_imports: Vec::new(),
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
        undetermined_imports: graph.undetermined_imports.into_iter().collect(),
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
    // for a fixed source state, as pack's own contract promises. ids are
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

    pub(super) fn node(id: usize, rel: &str, contents: &str) -> Node {
        node_in(id, rel, contents, Language::Rust)
    }

    pub(super) fn node_in(id: usize, rel: &str, contents: &str, language: Language) -> Node {
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
}
