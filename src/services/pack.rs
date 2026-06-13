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

    let graph = build_import_graph(&nodes);
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
    aliases: Vec<String>,
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
    for entry in file_filter.walk_builder().build().filter_map(|e| e.ok()) {
        let abs_path = entry.path();
        if !abs_path.is_file() {
            continue;
        }
        if !file_filter.should_include(abs_path) {
            continue;
        }
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
        let aliases = derive_aliases(abs_path, root);

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
                aliases,
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
                aliases: aliases.clone(),
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
            aliases,
            imports,
            signatures,
        });
    }

    // Canonical node order: the ignore::Walk yields entries in filesystem
    // readdir order (walk_builder does not set sort_by_file_path), which is
    // machine-dependent. Sort by rel_path — unique among nodes — and renumber
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

/// Module aliases worth matching against import targets. Keep them generous
/// — false positives become low-weight edges, not catastrophic ones.
fn derive_aliases(abs_path: &Path, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let stem = abs_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if !stem.is_empty() && stem != "mod" && stem != "index" {
        out.push(stem.to_string());
    }

    let rel = abs_path.strip_prefix(root).unwrap_or(abs_path);
    let mut path_components: Vec<String> = rel
        .iter()
        .filter_map(|c| c.to_str().map(|s| s.to_string()))
        .collect();

    if let Some(last) = path_components.last_mut()
        && let Some((stem, _ext)) = last.rsplit_once('.')
    {
        *last = stem.to_string();
    }

    if path_components.len() >= 2 {
        let joined = path_components.join("::");
        if !out.contains(&joined) {
            out.push(joined);
        }
        if path_components.last().is_some_and(|p| p == "mod") {
            // Rust: `src/services/lsp/mod.rs` is referenced as `services::lsp`
            let trimmed = &path_components[..path_components.len() - 1];
            let joined = trimmed.join("::");
            if !out.contains(&joined) {
                out.push(joined);
            }
        }
    }

    out
}

// --- graph -----------------------------------------------------------------

// BTreeMap, not HashMap: PageRank accumulates f64 scores by iterating this
// graph, and float addition is non-associative — a random HashMap iteration
// order would make scores differ by a ULP run-to-run, breaking pack's
// reproducibility contract. Ascending-id iteration is the canonical order.
type Graph = BTreeMap<usize, Vec<usize>>;

fn build_import_graph(nodes: &[Node]) -> Graph {
    let mut alias_index: HashMap<&str, Vec<usize>> = HashMap::new();
    for node in nodes {
        for alias in &node.aliases {
            alias_index.entry(alias.as_str()).or_default().push(node.id);
        }
    }

    let mut graph: Graph = BTreeMap::new();
    for node in nodes {
        graph.entry(node.id).or_default();
        let mut seen = HashSet::new();
        for target in &node.imports {
            for token in tokenize_import(target) {
                if let Some(matches) = alias_index.get(token.as_str()) {
                    for &dst in matches {
                        if dst != node.id && seen.insert(dst) {
                            graph.entry(node.id).or_default().push(dst);
                        }
                    }
                }
            }
        }
    }
    graph
}

/// Pull import paths out of a source file using deliberately conservative,
/// deterministic textual rules. False negatives are OK (a missed edge just
/// lowers a PageRank score); false positives across project boundaries
/// don't matter because we only look up against this project's alias index.
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
                    out.push(name.trim().to_string());
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

/// Split an import path text into the module identifiers worth matching.
fn tokenize_import(raw: &str) -> Vec<String> {
    let cleaned = raw
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`');
    let normalized = cleaned.replace(['/', '.'], "::").replace("::::", "::");
    let parts: Vec<&str> = normalized
        .split("::")
        .filter(|s| !s.is_empty() && *s != "crate" && *s != "self" && *s != "super" && *s != ".")
        .collect();

    let mut out = Vec::new();
    if let Some(last) = parts.last() {
        out.push((*last).to_string());
    }
    if parts.len() >= 2 {
        out.push(parts.join("::"));
        out.push(parts[..parts.len() - 1].join("::"));
    }
    out
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
            .filter(|n| {
                n.rel_path.to_lowercase().contains(&focus_lower)
                    || n.aliases
                        .iter()
                        .any(|a| a.to_lowercase().contains(&focus_lower))
            })
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

    fn node(id: usize, rel: &str, aliases: &[&str], contents: &str) -> Node {
        let cfg = PackConfig::default();
        Node {
            id,
            rel_path: rel.to_string(),
            language: Language::Rust,
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            imports: extract_imports(contents, Language::Rust),
            signatures: top_signatures_from(contents, Language::Rust, &cfg),
        }
    }

    #[test]
    fn aliases_for_rust_module() {
        let aliases = derive_aliases(Path::new("/repo/src/services/pack.rs"), Path::new("/repo"));
        assert!(aliases.contains(&"pack".to_string()));
        assert!(aliases.contains(&"src::services::pack".to_string()));
    }

    #[test]
    fn aliases_for_rust_mod_file() {
        let aliases = derive_aliases(
            Path::new("/repo/src/services/lsp/mod.rs"),
            Path::new("/repo"),
        );
        assert!(aliases.contains(&"src::services::lsp".to_string()));
        assert!(!aliases.iter().any(|a| a == "mod"));
    }

    #[test]
    fn tokenize_rust_use_path() {
        let toks = tokenize_import("crate::services::pack");
        assert!(toks.contains(&"pack".to_string()));
        assert!(toks.contains(&"services::pack".to_string()));
        assert!(!toks.contains(&"crate".to_string()));
    }

    #[test]
    fn tokenize_typescript_string_path() {
        let toks = tokenize_import("\"./services/pack\"");
        assert!(toks.contains(&"pack".to_string()));
        assert!(toks.contains(&"services::pack".to_string()));
    }

    #[test]
    fn tokenize_python_dotted_module() {
        let toks = tokenize_import("foo.bar.baz");
        assert!(toks.contains(&"baz".to_string()));
        assert!(toks.contains(&"foo::bar::baz".to_string()));
    }

    #[test]
    fn extract_rust_use_paths() {
        let src = "use crate::services::pack;\npub use crate::cli::output;\nmod auth;\n";
        let imports = extract_imports(src, Language::Rust);
        assert!(imports.iter().any(|s| s.contains("services::pack")));
        assert!(imports.iter().any(|s| s.contains("cli::output")));
        assert!(imports.iter().any(|s| s == "auth"));
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
            node(0, "src/auth.rs", &["auth"], ""),
            node(1, "src/render.rs", &["render"], ""),
            node(2, "src/util.rs", &["util"], ""),
        ];
        let pers = personalization_vector(&nodes, Some("auth"));
        assert!(pers[&0] > pers[&1]);
        assert!(pers[&0] > pers[&2]);
    }

    #[test]
    fn personalization_uniform_when_focus_missing() {
        let nodes = vec![
            node(0, "src/auth.rs", &["auth"], ""),
            node(1, "src/render.rs", &["render"], ""),
        ];
        let pers = personalization_vector(&nodes, None);
        assert!((pers[&0] - pers[&1]).abs() < 1e-9);
    }

    #[test]
    fn personalization_falls_back_to_uniform_on_no_match() {
        let nodes = vec![
            node(0, "src/auth.rs", &["auth"], ""),
            node(1, "src/render.rs", &["render"], ""),
        ];
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
                &["a"],
                &format!("{big_signature}\n{big_signature}\n{big_signature}\n"),
            ),
            node(1, "src/b.rs", &["b"], "pub fn small() {}\n"),
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
            node(0, "src/low.rs", &["low"], "pub fn low() {}\n"),
            node(1, "src/high.rs", &["high"], "pub fn high() {}\n"),
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
            node(0, "src/zebra.rs", &["z"], "pub fn z() {}\n"),
            node(1, "src/alpha.rs", &["a"], "pub fn a() {}\n"),
        ];
        let ranks: BTreeMap<usize, f64> = [(0, 0.5), (1, 0.5)].into();
        let result = fit_to_budget(&nodes, &ranks, 10_000);
        assert_eq!(result.files[0].path, PathBuf::from("src/alpha.rs"));
        assert_eq!(result.files[1].path, PathBuf::from("src/zebra.rs"));
    }

    #[test]
    fn import_graph_links_use_to_target() {
        let a = node(
            0,
            "src/a.rs",
            &["a", "src::a"],
            "use crate::b;\npub fn a() {}\n",
        );
        let b = node(1, "src/b.rs", &["b", "src::b"], "pub fn b() {}\n");
        let graph = build_import_graph(&[a, b]);
        assert!(graph[&0].contains(&1));
        assert!(graph[&1].is_empty());
    }
}
