use std::collections::HashSet;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::OutputContext;
use crate::cli::OutputError;
use crate::cli::response::Section;
use crate::cli::symbol_discovery::{
    broad_symbol_kind_bonus, coverage_reason, generic_exact_identifier_penalty,
    is_probably_test_path, noisy_suffix_penalty, symbol_lookup_hints, symbol_match_priority,
};
use crate::error::{LspError, StoreError};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol, SymbolKind};
use crate::services::store::{SymbolExtractor, SymbolSearchResult};

use super::common::{looks_like_symbol_path, resolve_search_languages};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SymbolResultOutput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub score: f64,
}

pub async fn execute_symbol_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    kind: Option<&str>,
    workspace_symbols: bool,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;

    let query = query.trim();
    if query.is_empty() {
        ctx.print_error(OutputError::invalid("Search query cannot be empty"));
        return Ok(());
    }

    // `*` wildcards are the file-tree matcher's syntax; neither the LIKE index
    // nor the LSP workspace search honors them (they would fuzzy-strip the star
    // and surface unrelated crates), so resolve a glob against the index with
    // our own matcher instead of routing it to either.
    if query.contains('*') {
        return execute_glob_symbol_search(app, query, kind, limit).await;
    }

    let search_languages = resolve_search_languages(app, language);
    let use_workspace = workspace_symbols || looks_like_symbol_path(query);

    if use_workspace {
        let route = if workspace_symbols {
            WorkspaceSearchRoute::Forced
        } else {
            WorkspaceSearchRoute::PathQuery
        };
        return execute_workspace_symbol_search(app, query, kind, limit, &search_languages, route)
            .await;
    }

    match app
        .store
        .search_symbols(query, limit, kind.map(SymbolKind::parse_or_default))
        .await
    {
        Ok(page) => {
            let mut count = page.total;
            let stale = page.stale;
            let mut candidates: Vec<SymbolResultOutput> = page
                .rows
                .into_iter()
                .map(|r| index_result_output(r, ctx))
                .collect();
            let mut failures = Vec::new();
            let mut indexing = None;
            if !search_languages.is_empty() && candidates.len() < limit {
                let lookup =
                    collect_workspace_symbol_results(app, query, kind, limit, &search_languages)
                        .await;
                candidates = merge_symbol_results(candidates, lookup.results, query);
                failures = lookup.failures;
                indexing = lookup.indexing;
                count = count.max(candidates.len());
            }
            let mut section = finish_symbol_search(candidates, count, query, language, kind, limit)
                .with_stale(stale)
                .with_indexing(indexing);
            if section.count == 0 {
                let hints = symbol_search_coverage_hints(&failures);
                if !hints.is_empty() {
                    section = section
                        .with_hints(hints)
                        .with_next_commands(symbol_search_coverage_next_commands(query, &failures));
                }
            }
            ctx.print_success(section);
        }
        // No index yet: answer from live LSP workspace symbols instead.
        Err(StoreError::NotInitialized) => {
            return execute_workspace_symbol_search(
                app,
                query,
                kind,
                limit,
                &search_languages,
                WorkspaceSearchRoute::IndexNotBuilt,
            )
            .await;
        }
        Err(e) => ctx.print_error(OutputError::internal(e.to_string())),
    }

    Ok(())
}

/// Resolve a `*`-glob query against the index: seed the substring search with
/// the pattern's longest literal run, then keep only the rows whose path the
/// shared matcher accepts. The index can't `LIKE`-glob and the LSP workspace
/// search can't honor `*` at all, so this is the one path that globs against
/// real project symbols without fuzzy-stripped noise.
async fn execute_glob_symbol_search(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;

    let seed = query
        .split(['*', '/', '[', ']'])
        .filter(|s| !s.is_empty())
        .max_by_key(|s| s.len())
        .unwrap_or("");

    // Scan every seed match — `usize::MAX` lands as SQLite `LIMIT -1`
    // (unlimited) — so the glob `count` is the exact total rather than a
    // page-capped lower bound. A literal seed keeps the set small; only a bare
    // `*` (empty seed) walks the whole index, which is exactly what `*` asks
    // for. `finish_symbol_search` caps the emitted page at the display `limit`.
    let page = match app
        .store
        .search_symbols(seed, usize::MAX, kind.map(SymbolKind::parse_or_default))
        .await
    {
        Ok(page) => page,
        Err(StoreError::NotInitialized) => {
            ctx.print_error(
                OutputError::not_found("Search index not built")
                    .with_hint("Run 'symora search index build', then retry the wildcard query"),
            );
            return Ok(());
        }
        Err(e) => {
            ctx.print_error(OutputError::internal(e.to_string()));
            return Ok(());
        }
    };

    let stale = page.stale;
    // Keep every glob match for an accurate count; finish_symbol_search caps
    // the emitted page at `limit` and sets `truncated`.
    let matches: Vec<SymbolResultOutput> = page
        .rows
        .into_iter()
        .filter(|r| Symbol::path_matches(r.name_path.as_deref().unwrap_or(&r.name), query))
        .map(|r| index_result_output(r, ctx))
        .collect();
    let count = matches.len();

    ctx.print_success(
        finish_symbol_search(matches, count, query, None, kind, limit).with_stale(stale),
    );
    Ok(())
}

/// Map a raw index row to the emitted output shape, relativizing the path
/// and tagging the backend.
fn index_result_output(row: SymbolSearchResult, ctx: &OutputContext) -> SymbolResultOutput {
    SymbolResultOutput {
        name: row.name,
        name_path: row.name_path,
        kind: row.kind.to_string(),
        file: ctx.relative_path(&row.file),
        line: row.line,
        column: row.column,
        container: row.container,
        backend: Some("index".to_string()),
        score: row.score,
    }
}

/// Which entry point routed the call to live workspace symbols. An empty
/// result's failure disclosure keys its remedy off this — the honest next
/// command depends on why the index was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceSearchRoute {
    /// `--workspace-symbols` explicitly skipped the index.
    Forced,
    /// The store reported `NotInitialized`; the live lookup is the
    /// fallback and the zero is workspace-only.
    IndexNotBuilt,
    /// A path-like query routed here; a built index still supplements
    /// the live results in the same call.
    PathQuery,
}

async fn execute_workspace_symbol_search(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
    route: WorkspaceSearchRoute,
) -> Result<()> {
    let ctx = &app.output;
    if languages.is_empty() {
        ctx.print_error(OutputError::not_found(
            "No project languages detected for workspace symbol search",
        ));
        return Ok(());
    }

    let lookup = collect_workspace_symbol_results(app, query, kind, limit, languages).await;
    let mut candidates = lookup.results;
    let failures = lookup.failures;
    let indexing = lookup.indexing;
    let mut count = candidates.len();
    let mut stale = false;
    let mut index_answered = false;

    if looks_like_symbol_path(query) && candidates.len() < limit {
        let expanded =
            collect_document_path_results(app, query, kind, limit, languages, &candidates).await;
        candidates = merge_symbol_results(candidates, expanded, query);
    }

    if looks_like_symbol_path(query)
        && let Ok(page) = app
            .store
            .search_symbols(query, limit, kind.map(SymbolKind::parse_or_default))
            .await
    {
        index_answered = true;
        count = count.max(page.total);
        // The merged output contains index rows, so the page's staleness
        // applies to it; the live workspace results are current by nature.
        stale = page.stale;
        let index_results: Vec<SymbolResultOutput> = page
            .rows
            .into_iter()
            .map(|r| index_result_output(r, ctx))
            .collect();
        candidates = merge_symbol_results(candidates, index_results, query);
    }

    count = count.max(candidates.len());
    let section = with_workspace_failure_disclosure(
        finish_symbol_search(candidates, count, query, None, kind, limit)
            .with_stale(stale)
            // Any emitted language having run timed-out makes the whole
            // list a lower bound — captured when each query ran.
            .with_indexing(indexing),
        &failures,
        query,
        route,
        index_answered,
    );
    ctx.print_success(section);
    Ok(())
}

/// Empty results disclose this call's failed lookups — every language
/// whose zero the call cannot vouch for is a coverage gap. When the index
/// supplement answered in this same call, extractor-covered languages are
/// answered regardless of LSP state, so only `uncovered_language_failures`
/// qualify — the same gate and vocabulary as the index path. Otherwise the
/// zero is workspace-only and every failure is a gap, with the remedy
/// keyed to the route. Non-empty results stay bare, and a clean empty
/// stays a genuine zero.
fn with_workspace_failure_disclosure(
    section: Section<SymbolResultOutput>,
    failures: &[(Language, LspError)],
    query: &str,
    route: WorkspaceSearchRoute,
    index_answered: bool,
) -> Section<SymbolResultOutput> {
    if section.count != 0 || failures.is_empty() {
        return section;
    }
    if index_answered {
        let hints = symbol_search_coverage_hints(failures);
        if hints.is_empty() {
            return section;
        }
        let next_commands = symbol_search_coverage_next_commands(query, failures);
        return section.with_hints(hints).with_next_commands(next_commands);
    }
    section
        .with_hints(workspace_symbol_failure_hints(failures))
        .with_next_commands(workspace_symbol_failure_next_commands(
            query, failures, route,
        ))
}

/// Final shaping shared by both search paths: suppress low-value noise,
/// cap emission at `limit`, and derive `truncated`/hints from the exact
/// candidate count — never from limit saturation.
fn finish_symbol_search(
    mut candidates: Vec<SymbolResultOutput>,
    count: usize,
    query: &str,
    language: Option<&str>,
    kind: Option<&str>,
    limit: usize,
) -> Section<SymbolResultOutput> {
    prune_low_value_symbol_results(&mut candidates, query, limit);
    candidates.truncate(limit);

    let truncated = candidates.len() < count;
    let hints = symbol_search_hints(query, language, kind, truncated, candidates.len());
    let next_commands = symbol_search_next_commands(&candidates, query, language);
    Section::with_total(candidates, count)
        .with_hints(hints)
        .with_next_commands(next_commands)
}

/// Workspace-symbol fan-out outcome: the ranked results plus each failed
/// language with its error, so an empty result can disclose which
/// languages it does not actually cover instead of swallowing them.
/// `indexing` is present when any answering language's query ran under
/// degraded indexing — the combined result is then a lower bound.
struct WorkspaceSymbolLookup {
    results: Vec<SymbolResultOutput>,
    failures: Vec<(Language, LspError)>,
    indexing: Option<crate::models::lsp::IndexingDegradation>,
}

async fn collect_workspace_symbol_results(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
) -> WorkspaceSymbolLookup {
    let ctx = &app.output;
    let parsed_kind = kind.map(crate::models::symbol::SymbolKind::parse_or_default);
    let mut failures = Vec::new();
    let mut indexing = None;
    if languages.is_empty() {
        return WorkspaceSymbolLookup {
            results: Vec::new(),
            failures,
            indexing,
        };
    }

    let workspace_query = workspace_query_from_pattern(query);
    let overfetch_limit = if looks_like_symbol_path(query) {
        limit
    } else {
        limit.saturating_mul(4)
    };
    let mut seen = HashSet::new();
    let mut results = Vec::new();

    for language in languages {
        let mut symbols = match app.lsp.workspace_symbols(&workspace_query, *language).await {
            Ok(symbols) => {
                if indexing.is_none() {
                    indexing = symbols.indexing;
                }
                symbols.data
            }
            Err(e) => {
                failures.push((*language, e));
                continue;
            }
        };

        for symbol in &mut symbols {
            if let Some(path) = symbol.workspace_name_path() {
                symbol.name_path = Some(path);
            }
        }

        let filtered = if looks_like_symbol_path(query) {
            crate::models::symbol::Symbol::filter_advanced(
                &symbols,
                Some(query),
                false,
                parsed_kind.as_ref().map(std::slice::from_ref),
                None,
                false,
            )
        } else {
            symbols
                .into_iter()
                .filter(|symbol| {
                    kind_matches(symbol, parsed_kind.as_ref())
                        && symbol
                            .name
                            .to_ascii_lowercase()
                            .contains(&query.to_ascii_lowercase())
                })
                .collect()
        };

        for symbol in filtered {
            let key = format!(
                "{}:{}:{}:{}",
                symbol.location.file.display(),
                symbol.location.line,
                symbol.location.column,
                symbol.name
            );
            if seen.insert(key) {
                results.push(symbol);
            }
        }

        if results.len() >= overfetch_limit.saturating_mul(2) {
            break;
        }
    }

    results.sort_by(|a, b| {
        score_workspace_symbol(query, b)
            .partial_cmp(&score_workspace_symbol(query, a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut outputs: Vec<_> = results
        .into_iter()
        .take(overfetch_limit)
        .map(|symbol| {
            let score = score_workspace_symbol(query, &symbol);
            SymbolResultOutput {
                name: symbol.name,
                name_path: symbol.name_path,
                kind: symbol.kind.to_string(),
                file: ctx.relative_path(&symbol.location.file),
                line: symbol.location.line,
                column: symbol.location.column,
                container: symbol.container,
                backend: Some("workspace".to_string()),
                score,
            }
        })
        .collect();
    sort_symbol_results(&mut outputs, query);
    prune_low_value_symbol_results(&mut outputs, query, limit);
    WorkspaceSymbolLookup {
        results: outputs,
        failures,
        indexing,
    }
}

/// Dedup + rank the union of two result sets. Emission capping and noise
/// suppression happen once, in `finish_symbol_search`, so the candidate
/// count stays exact.
fn merge_symbol_results(
    primary: Vec<SymbolResultOutput>,
    secondary: Vec<SymbolResultOutput>,
    query: &str,
) -> Vec<SymbolResultOutput> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();

    for result in primary.into_iter().chain(secondary) {
        let symbol_key = result.name_path.as_deref().unwrap_or(&result.name);
        let key = format!("{}:{}:{}", result.file, result.line, symbol_key,);
        if seen.insert(key) {
            merged.push(result);
        }
    }

    sort_symbol_results(&mut merged, query);
    merged
}

async fn collect_document_path_results(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
    seed_results: &[SymbolResultOutput],
) -> Vec<SymbolResultOutput> {
    let ctx = &app.output;
    let parsed_kind = kind.map(crate::models::symbol::SymbolKind::parse_or_default);
    let leaf = workspace_query_from_pattern(query);
    if leaf.is_empty() {
        return Vec::new();
    }

    let mut candidate_files = Vec::new();
    let mut seen_files = HashSet::new();

    for result in seed_results {
        let file = app.root().join(&result.file);
        if seen_files.insert(file.clone()) {
            candidate_files.push(file);
        }
    }

    if candidate_files.len() < limit {
        let workspace_seeds =
            collect_workspace_symbol_results(app, &leaf, kind, limit * 2, languages)
                .await
                .results;
        for result in workspace_seeds {
            let file = app.root().join(&result.file);
            if seen_files.insert(file.clone()) {
                candidate_files.push(file);
                if candidate_files.len() >= limit * 2 {
                    break;
                }
            }
        }
    }

    let mut expanded = Vec::new();
    let mut seen_symbols = HashSet::new();
    for file in candidate_files.into_iter().take(limit * 2) {
        let Ok(mut symbols) = app
            .lsp
            .find_symbols(&file, FindSymbolsOptions::default().with_depth(10))
            .await
        else {
            continue;
        };

        crate::models::symbol::Symbol::compute_paths_for_all(&mut symbols);
        let matched = crate::models::symbol::Symbol::filter_advanced(
            &symbols,
            Some(query),
            false,
            parsed_kind.as_ref().map(std::slice::from_ref),
            None,
            false,
        );

        for symbol in matched {
            let file_rel = ctx.relative_path(&symbol.location.file);
            let key = format!(
                "{}:{}:{}:{}",
                file_rel,
                symbol.location.line,
                symbol.location.column,
                symbol.path()
            );
            if seen_symbols.insert(key) {
                expanded.push(SymbolResultOutput {
                    score: score_workspace_symbol(query, &symbol),
                    name: symbol.name,
                    name_path: symbol.name_path,
                    kind: symbol.kind.to_string(),
                    file: file_rel,
                    line: symbol.location.line,
                    column: symbol.location.column,
                    container: symbol.container,
                    backend: Some("document".to_string()),
                });
            }
        }
    }

    sort_symbol_results(&mut expanded, query);
    prune_low_value_symbol_results(&mut expanded, query, limit);
    expanded.truncate(limit);
    expanded
}

fn sort_symbol_results(results: &mut [SymbolResultOutput], query: &str) {
    results.sort_by(|a, b| {
        symbol_result_priority(query, b)
            .cmp(&symbol_result_priority(query, a))
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
}

fn prune_low_value_symbol_results(
    results: &mut Vec<SymbolResultOutput>,
    query: &str,
    limit: usize,
) {
    if looks_like_symbol_path(query) || results.is_empty() {
        return;
    }

    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let high_value_count = results
        .iter()
        .filter(|result| is_high_value_symbol_result(result, &q))
        .count();

    if high_value_count >= usize::min(limit, 3) {
        results.retain(|result| is_high_value_symbol_result(result, &q));
    }
}

fn symbol_result_priority(query: &str, result: &SymbolResultOutput) -> i32 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let name = result.name.to_ascii_lowercase();
    let path = result
        .name_path
        .as_deref()
        .unwrap_or(&result.name)
        .to_ascii_lowercase();
    let match_priority = symbol_match_priority(query, &name, &path);

    // Test code is demoted by file path only. A container-name substring
    // check ("test") would mis-fire on Fastest/Latest/Contest, and the file
    // path already catches the real test code.
    let test_penalty = if is_probably_test_path(&result.file) {
        8
    } else {
        0
    };
    let kind_penalty = if is_low_signal_kind(&result.kind) {
        6
    } else {
        0
    };
    let suffix_penalty = noisy_suffix_penalty(&name, &q);
    // For a broad single-word query, prefer a high-signal type/function
    // whose name contains the term (+8) over a low-signal exact match such
    // as a same-named variable or enum member (-24/-18), which is rarely
    // what the agent is looking for.
    let generic_exact_penalty = generic_exact_identifier_penalty(
        query,
        &name,
        &result.kind,
        is_low_signal_kind(&result.kind),
    );
    let kind_bonus =
        broad_symbol_kind_bonus(query, &name, &result.kind, is_low_signal_kind(&result.kind));

    match_priority + kind_bonus
        - test_penalty
        - kind_penalty
        - suffix_penalty
        - generic_exact_penalty
}

fn is_high_value_symbol_result(result: &SymbolResultOutput, query: &str) -> bool {
    let name = result.name.to_ascii_lowercase();
    !is_probably_test_path(&result.file)
        && !is_low_signal_kind(&result.kind)
        && noisy_suffix_penalty(&name, query) == 0
}

fn is_low_signal_kind(kind: &str) -> bool {
    matches!(
        kind,
        "variable" | "field" | "property" | "enum_member" | "constant"
    )
}

fn workspace_query_from_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim().trim_start_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let base = last.split('[').next().unwrap_or(last);
    base.trim_matches('*').to_string()
}

fn kind_matches(
    symbol: &crate::models::symbol::Symbol,
    kind: Option<&crate::models::symbol::SymbolKind>,
) -> bool {
    kind.is_none_or(|expected| &symbol.kind == expected)
}

fn score_workspace_symbol(query: &str, symbol: &crate::models::symbol::Symbol) -> f64 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let name = symbol.name.to_ascii_lowercase();
    let path = symbol
        .name_path
        .as_deref()
        .unwrap_or(&symbol.name)
        .to_ascii_lowercase();
    let last = q.rsplit('/').next().unwrap_or(&q);

    if looks_like_symbol_path(query) {
        if path == q {
            1.0
        } else if path.ends_with(&format!("/{q}")) {
            0.96
        } else if name == last {
            0.78
        } else if path.contains(last) {
            0.68
        } else {
            0.5
        }
    } else if name == q {
        1.0
    } else if path.ends_with(&format!("/{q}")) {
        0.95
    } else if name.starts_with(&q) {
        0.9
    } else if name.contains(&q) {
        0.75
    } else {
        0.5
    }
}

fn symbol_search_hints(
    query: &str,
    language: Option<&str>,
    kind: Option<&str>,
    truncated: bool,
    result_count: usize,
) -> Vec<String> {
    symbol_lookup_hints(
        query,
        looks_like_symbol_path(query),
        language.is_none(),
        kind.is_none(),
        truncated,
        result_count,
    )
}

/// Failed languages whose zero an empty result cannot vouch for: no
/// compiled-in index extractor AND this call's workspace-symbol lookup
/// failed. Extractor-covered languages are answered by the index
/// regardless of LSP state, so they never qualify. Sorted by language id
/// for deterministic output.
fn uncovered_language_failures(failures: &[(Language, LspError)]) -> Vec<&(Language, LspError)> {
    let mut uncovered: Vec<_> = failures
        .iter()
        .filter(|(language, _)| !SymbolExtractor::is_supported(*language))
        .collect();
    uncovered.sort_by_key(|(language, _)| language.lsp_id());
    uncovered
}

fn symbol_search_coverage_hints(failures: &[(Language, LspError)]) -> Vec<String> {
    let mut hints: Vec<String> = uncovered_language_failures(failures)
        .into_iter()
        .map(|(language, err)| {
            format!(
                "This zero does not cover {lang}: {lang} has no index symbol extractor and its language server is unavailable ({reason})",
                lang = language.lsp_id(),
                reason = coverage_reason(err)
            )
        })
        .collect();
    hints.truncate(2);
    hints
}

fn symbol_search_coverage_next_commands(
    query: &str,
    failures: &[(Language, LspError)],
) -> Vec<String> {
    let mut commands: Vec<String> = uncovered_language_failures(failures)
        .first()
        .map(|(language, _)| {
            let lang = language.lsp_id();
            vec![
                format!("symora search content '{query}' --lang {lang}"),
                format!("symora doctor {lang}"),
            ]
        })
        .unwrap_or_default();
    commands.truncate(2);
    commands
}

/// Sorted by language id for deterministic output, capped like the
/// index-path coverage hints.
fn workspace_symbol_failure_hints(failures: &[(Language, LspError)]) -> Vec<String> {
    let mut sorted: Vec<_> = failures.iter().collect();
    sorted.sort_by_key(|(language, _)| language.lsp_id());
    let mut hints: Vec<String> = sorted
        .into_iter()
        .map(|(language, err)| {
            format!(
                "This zero does not cover {lang}: its workspace symbol lookup failed ({reason})",
                lang = language.lsp_id(),
                reason = coverage_reason(err)
            )
        })
        .collect();
    hints.truncate(2);
    hints
}

/// Route-appropriate remedies for a workspace-only zero, keyed on the
/// first failed language: a forced live lookup is cured by dropping the
/// flag so the index can answer; an unbuilt index is cured by building it
/// — but only for a language the extractor covers. `search index build`
/// can never help a language with no extractor, so those steer to content
/// search instead, like the index path's coverage commands.
fn workspace_symbol_failure_next_commands(
    query: &str,
    failures: &[(Language, LspError)],
    route: WorkspaceSearchRoute,
) -> Vec<String> {
    let mut sorted: Vec<_> = failures.iter().collect();
    sorted.sort_by_key(|(language, _)| language.lsp_id());
    let Some((language, _)) = sorted.first() else {
        return Vec::new();
    };
    let lang = language.lsp_id();
    match route {
        WorkspaceSearchRoute::Forced => vec![
            format!("symora search symbols '{query}'"),
            format!("symora doctor {lang}"),
        ],
        WorkspaceSearchRoute::IndexNotBuilt | WorkspaceSearchRoute::PathQuery => {
            if SymbolExtractor::is_supported(*language) {
                vec![
                    "symora search index build".to_string(),
                    format!("symora doctor {lang}"),
                ]
            } else {
                vec![
                    format!("symora search content '{query}' --lang {lang}"),
                    format!("symora doctor {lang}"),
                ]
            }
        }
    }
}

fn symbol_search_next_commands(
    results: &[SymbolResultOutput],
    query: &str,
    language: Option<&str>,
) -> Vec<String> {
    if !needs_symbol_follow_up(results) {
        return Vec::new();
    }

    let mut commands = Vec::new();
    if let Some(first) = results.first() {
        let lang_flag = language
            .map(|lang| format!(" --lang {lang}"))
            .unwrap_or_default();
        let symbol_key = first.name_path.as_deref().unwrap_or(&first.name);
        commands.push(format!(
            "symora symbols {} --symbol '{}' --depth 2",
            first.file, symbol_key
        ));
        commands.push(format!("symora map file {} --related-limit 5", first.file));
        if !looks_like_symbol_path(query) {
            commands.push(format!(
                "symora search symbols '{}'{} --kind {}",
                query, lang_flag, first.kind
            ));
        }
    }
    commands.truncate(3);
    commands
}

fn needs_symbol_follow_up(results: &[SymbolResultOutput]) -> bool {
    if results.len() <= 1 {
        return false;
    }

    let first = &results[0];
    let second = &results[1];
    let score_close = (first.score - second.score).abs() < 0.15;
    let different_file = first.file != second.file;
    let different_symbol = first.name_path != second.name_path || first.kind != second.kind;

    (score_close && different_symbol) || different_file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_hints_are_empty_for_exact_single_result() {
        let hints = symbol_search_hints("SearchCommand/Content", None, None, false, 1);
        assert!(hints.is_empty());
    }

    fn result(name: &str, file: &str) -> SymbolResultOutput {
        SymbolResultOutput {
            name: name.to_string(),
            name_path: Some(name.to_string()),
            kind: "function".to_string(),
            file: file.to_string(),
            line: 1,
            column: 1,
            container: None,
            backend: Some("index".to_string()),
            score: 1.0,
        }
    }

    #[test]
    fn finish_symbol_search_derives_truncation_from_exact_count() {
        let candidates = vec![
            result("alpha", "src/a.rs"),
            result("beta", "src/b.rs"),
            result("gamma", "src/c.rs"),
        ];
        let section = finish_symbol_search(candidates, 3, "alpha", None, None, 2);

        assert_eq!(section.count, 3);
        assert_eq!(section.showing, 2);
        assert!(section.truncated);
    }

    #[test]
    fn finish_symbol_search_complete_results_are_not_truncated() {
        let candidates = vec![result("alpha", "src/a.rs")];
        let section = finish_symbol_search(candidates, 1, "alpha", None, None, 10);

        assert_eq!(section.count, 1);
        assert_eq!(section.showing, 1);
        assert!(!section.truncated);
    }

    #[test]
    fn coverage_hint_fires_for_uncovered_language_with_failed_server() {
        let failures = vec![(
            Language::Lua,
            LspError::ServerNotInstalled {
                name: "lua-language-server".to_string(),
                install_hint: "brew install lua-language-server".to_string(),
            },
        )];

        let hints = symbol_search_coverage_hints(&failures);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("lua"));
        assert!(hints[0].contains("server_not_installed"));

        assert_eq!(
            symbol_search_coverage_next_commands("q", &failures),
            vec!["symora search content 'q' --lang lua", "symora doctor lua"]
        );
    }

    #[test]
    fn coverage_hint_silent_for_extractor_language() {
        // Rust is index-covered: a failed server does not make its zero
        // non-exhaustive, so no disclosure fires.
        let failures = vec![(
            Language::Rust,
            LspError::ServerNotInstalled {
                name: "rust-analyzer".to_string(),
                install_hint: "rustup component add rust-analyzer".to_string(),
            },
        )];
        assert!(symbol_search_coverage_hints(&failures).is_empty());
        assert!(symbol_search_coverage_next_commands("q", &failures).is_empty());
    }

    #[test]
    fn coverage_hint_silent_with_no_failures() {
        assert!(symbol_search_coverage_hints(&[]).is_empty());
        assert!(symbol_search_coverage_next_commands("q", &[]).is_empty());
    }

    fn server_failure(language: Language) -> (Language, LspError) {
        (
            language,
            LspError::ServerNotInstalled {
                name: "server".to_string(),
                install_hint: "install it".to_string(),
            },
        )
    }

    /// A built index covering rust + the rust LSP failing + a path-like query:
    /// the index answered in this same call, so its zero covers rust — no
    /// coverage claim, no index-build no-op.
    #[test]
    fn index_answered_route_stays_bare_for_extractor_covered_failure() {
        let failures = vec![server_failure(Language::Rust)];
        let section = with_workspace_failure_disclosure(
            Section::with_total(Vec::new(), 0),
            &failures,
            "Nonexistent/missing_xyz",
            WorkspaceSearchRoute::PathQuery,
            true,
        );
        assert!(section.hints.is_empty());
        assert!(section.next_commands.is_empty());
    }

    /// With the index answered, an extractor-less failure is a genuine
    /// gap and gets the index path's hint/command family — never a
    /// `search index build` that cannot cover the language.
    #[test]
    fn index_answered_route_steers_extractor_less_failure_to_content_search() {
        let failures = vec![server_failure(Language::Lua)];
        let section = with_workspace_failure_disclosure(
            Section::with_total(Vec::new(), 0),
            &failures,
            "Mod/handler",
            WorkspaceSearchRoute::PathQuery,
            true,
        );
        assert_eq!(section.hints.len(), 1);
        assert!(section.hints[0].contains("lua"));
        assert!(section.hints[0].contains("no index symbol extractor"));
        assert_eq!(
            section.next_commands,
            vec![
                "symora search content 'Mod/handler' --lang lua",
                "symora doctor lua"
            ]
        );
    }

    #[test]
    fn index_not_built_route_steers_extractor_covered_failure_to_index_build() {
        let failures = vec![server_failure(Language::Rust)];
        let section = with_workspace_failure_disclosure(
            Section::with_total(Vec::new(), 0),
            &failures,
            "alpha",
            WorkspaceSearchRoute::IndexNotBuilt,
            false,
        );
        assert_eq!(section.hints.len(), 1);
        assert!(section.hints[0].contains("rust"));
        assert!(section.hints[0].contains("workspace symbol lookup failed"));
        assert!(section.hints[0].contains("server_not_installed"));
        assert_eq!(
            section.next_commands,
            vec!["symora search index build", "symora doctor rust"]
        );
    }

    /// `search index build` can never help a language with no extractor;
    /// the unbuilt-index route steers those to content search instead.
    #[test]
    fn index_not_built_route_never_suggests_index_build_for_extractor_less_language() {
        let failures = vec![server_failure(Language::Lua)];
        let section = with_workspace_failure_disclosure(
            Section::with_total(Vec::new(), 0),
            &failures,
            "alpha",
            WorkspaceSearchRoute::IndexNotBuilt,
            false,
        );
        assert_eq!(section.hints.len(), 1);
        assert!(section.hints[0].contains("lua"));
        assert_eq!(
            section.next_commands,
            vec![
                "symora search content 'alpha' --lang lua",
                "symora doctor lua"
            ]
        );
    }

    /// A forced live lookup skipped the index deliberately; the cure is
    /// dropping the flag so the index can answer, not rebuilding it.
    #[test]
    fn forced_route_suggests_dropping_the_flag() {
        let failures = vec![server_failure(Language::Rust)];
        let section = with_workspace_failure_disclosure(
            Section::with_total(Vec::new(), 0),
            &failures,
            "alpha",
            WorkspaceSearchRoute::Forced,
            false,
        );
        assert_eq!(section.hints.len(), 1);
        assert!(section.hints[0].contains("workspace symbol lookup failed"));
        assert_eq!(
            section.next_commands,
            vec!["symora search symbols 'alpha'", "symora doctor rust"]
        );
    }

    #[test]
    fn workspace_failure_disclosure_silent_on_empty_result_without_failures() {
        for route in [
            WorkspaceSearchRoute::Forced,
            WorkspaceSearchRoute::IndexNotBuilt,
            WorkspaceSearchRoute::PathQuery,
        ] {
            let section = with_workspace_failure_disclosure(
                Section::with_total(Vec::new(), 0),
                &[],
                "alpha",
                route,
                false,
            );
            assert!(section.hints.is_empty());
            assert!(section.next_commands.is_empty());
        }
    }

    #[test]
    fn workspace_failure_disclosure_keeps_non_empty_results_bare() {
        let failures = vec![server_failure(Language::Rust)];
        let section = with_workspace_failure_disclosure(
            Section::with_total(vec![result("alpha", "src/a.rs")], 1),
            &failures,
            "alpha",
            WorkspaceSearchRoute::IndexNotBuilt,
            false,
        );
        assert!(section.hints.is_empty());
        assert!(section.next_commands.is_empty());
    }

    /// A workspace-symbol row must carry the same `name_path` the index and
    /// documentSymbol surfaces emit: a structural impl self type collapses to
    /// its nominal head (so a path copied from `search` resolves under
    /// `symbols`/`edit`), while plain and module-qualified containers keep
    /// their path.
    #[test]
    fn workspace_name_path_matches_the_index_self_type_segment() {
        use crate::models::symbol::Location;
        use std::path::PathBuf;
        let m = |container: &str| {
            Symbol::new(
                "tm".to_string(),
                SymbolKind::Method,
                Location::point(PathBuf::from("a.rs"), 1, 1),
            )
            .with_container(container)
            .workspace_name_path()
        };
        // structural self types collapse to their first nominal head
        assert_eq!(m("(Foo, Bar)").as_deref(), Some("Foo/tm"));
        assert_eq!(m("[Elem; 4]").as_deref(), Some("Elem/tm"));
        assert_eq!(m("*const Ptr").as_deref(), Some("Ptr/tm"));
        assert_eq!(m("<Qual as Baz>::Out").as_deref(), Some("Qual/tm"));
        // a module-qualified self type reduces to the bare type, exactly as the
        // index does — both call `self_type_segment` (`fn_mod::Named` → `Named`)
        assert_eq!(m("Language").as_deref(), Some("Language/tm"));
        assert_eq!(m("fn_mod::Named").as_deref(), Some("Named/tm"));
    }
}
