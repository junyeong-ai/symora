use std::collections::HashSet;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::OutputContext;
use crate::cli::OutputError;
use crate::cli::response::disclosure::{
    DisclosureRoute, LiveLookup, LowerBound, Uncovered, WorkspaceSearchRoute, coverage_shortfall,
    index_holes_bound, index_unavailable_disclosure, ordered_bounds, relative_stale_files,
    unconfirmed_by_live_lookup, unconfirmed_zero_fact, with_coverage_disclosure,
    workspace_route_for,
};
use crate::cli::response::{CoverageGap, Section};
use crate::cli::symbol_discovery::{
    DetectedLanguages, RankedSymbol, candidate_budget, no_languages_error,
    resolve_search_languages, symbol_lookup_hints, symbol_rank,
};
use crate::error::{LspError, StoreError};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol, SymbolKind};
use crate::services::TestScope;
use crate::services::store::SymbolSearchResult;

use super::common::looks_like_symbol_path;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SymbolResultOutput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// Present (true) only when `column` is a degraded wire-offset guess — a
    /// cross-file `workspace/symbol` result decoded against an unreadable line.
    /// Index and same-file results never set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_column: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub score: f64,
}

/// `stale` narrowed to the rows this section actually emitted.
///
/// The index page it was read from is a superset of them — ranking, the merge
/// with live rows, and the limit all cut into it — so a page holding one stale
/// row and one fresh one says nothing about an answer that kept only the fresh
/// one. A live row is current by nature, whatever its file did.
fn with_emitted_stale(
    section: Section<SymbolResultOutput>,
    stale_files: &std::collections::HashSet<String>,
) -> Section<SymbolResultOutput> {
    let stale = section
        .items
        .iter()
        .any(|item| item.backend.as_deref() == Some("index") && stale_files.contains(&item.file));
    section.with_stale(stale)
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

    // A zero cap cannot be answered honestly: the index reports the total
    // it saw through the window, so an empty window would publish a zero
    // for a repository full of matches. Ask for one result to learn the
    // count.
    if limit == 0 {
        ctx.print_error(
            OutputError::invalid("--limit must be at least 1")
                .with_hint("Use --limit 1 to learn the count from a single result."),
        );
        return Ok(());
    }

    if language.map(Language::parse_or_default) == Some(Language::Unknown) {
        ctx.print_error(
            OutputError::invalid(format!(
                "Unknown language: {}",
                language.unwrap_or_default()
            ))
            .with_hint("Run 'symora doctor' to see supported languages."),
        );
        return Ok(());
    }

    // `*` wildcards are the file-tree matcher's syntax; neither the LIKE index
    // nor the LSP workspace search honors them (they would fuzzy-strip the star
    // and surface unrelated crates), so resolve a glob against the index with
    // our own matcher instead of routing it to either.
    if query.contains('*') {
        return execute_glob_symbol_search(app, query, language, kind, limit).await;
    }

    let detected = resolve_search_languages(app, language);
    let use_workspace = workspace_symbols || looks_like_symbol_path(query);

    if use_workspace {
        let route = if workspace_symbols {
            WorkspaceSearchRoute::Forced
        } else {
            WorkspaceSearchRoute::PathQuery
        };
        return execute_workspace_symbol_search(
            app,
            query,
            language,
            kind,
            limit,
            WorkspaceRoute {
                route,
                detected: &detected,
                index_unavailable: None,
            },
        )
        .await;
    }

    match app
        .store
        .search_symbols(
            query,
            candidate_budget(limit),
            kind.map(SymbolKind::parse_or_default),
            explicit_index_language(language),
        )
        .await
    {
        Ok(page) => {
            let mut count = page.total;
            let stale_files = relative_stale_files(ctx, &page.stale_files);
            let mut candidates: Vec<SymbolResultOutput> = page
                .rows
                .into_iter()
                .map(|r| index_result_output(r, ctx))
                .collect();
            let mut failures = Vec::new();
            let mut skipped = Vec::new();
            let mut workspace_indexing = None;
            let covered = page.covered.clone();
            let lower_bounds = ordered_bounds(
                detected.shortfall(ctx),
                index_holes_bound(ctx, &page.unread_paths, &covered),
            );
            let uncovered = languages_needing_live_lookup(
                &detected.languages,
                &covered,
                !candidates.is_empty(),
            );
            if !uncovered.is_empty() {
                let lookup =
                    collect_workspace_symbol_results(app, query, kind, limit, &uncovered).await;
                workspace_indexing = lookup.indexing;
                let live_total = lookup.total;
                candidates = merge_symbol_results(candidates, lookup.results);
                failures = lookup.failures;
                skipped = lookup.skipped;
                // Disjoint by `languages_needing_live_lookup`, so the union
                // is the sum rather than the larger of the two — taking the
                // larger would report a hundred Rust matches as the whole
                // answer while a hundred Lua ones sat beside them, with
                // nothing in the coverage gaps to say so.
                count += live_total;
            }
            let shortfall = coverage_shortfall(
                &covered,
                LiveLookup::Ran {
                    failures: &failures,
                    skipped: &skipped,
                },
            );
            let unconfirmed_zero = unconfirmed_zero_fact(
                app.store.as_ref(),
                count,
                &unconfirmed_by_live_lookup(&covered, &failures, &skipped),
            )
            .await;
            let section = with_coverage_disclosure(
                with_emitted_stale(
                    finish_symbol_search(
                        candidates,
                        count,
                        query,
                        language,
                        kind,
                        limit,
                        app.test_scope(),
                        &shortfall,
                    ),
                    &stale_files,
                )
                .with_indexing(workspace_indexing),
                &shortfall,
                query,
                DisclosureRoute::IndexConsulted,
                &lower_bounds,
                &unconfirmed_zero,
            );
            ctx.print_success(section);
        }
        // Nothing to read the index for: never built, a build owns it, or it
        // could not be opened at all. Live LSP workspace symbols answer in its
        // place, and the route says which of the three it was — one is cured
        // by building, one by waiting, and one by neither.
        Err(e) => {
            // A daemon that was never reached says nothing about the store, so
            // there is nothing to answer around: it is reported as itself.
            let Some(route) = workspace_route_for(&e) else {
                ctx.print_error(OutputError::from(e));
                return Ok(());
            };
            return execute_workspace_symbol_search(
                app,
                query,
                language,
                kind,
                limit,
                WorkspaceRoute {
                    route,
                    detected: &detected,
                    index_unavailable: index_unavailable_disclosure(&e),
                },
            )
            .await;
        }
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
    language: Option<&str>,
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
        .search_symbols(
            seed,
            usize::MAX,
            kind.map(SymbolKind::parse_or_default),
            explicit_index_language(language),
        )
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
        // A wildcard is answered from the index alone, so a build in
        // progress leaves nothing to answer with — but telling the caller
        // to build one would be prescribing what is already running.
        Err(StoreError::Rebuilding) => {
            ctx.print_error(
                OutputError::from(StoreError::Rebuilding)
                    .with_hint("Wait for 'symora search index status' to report is_indexing: false, then retry the wildcard query"),
            );
            return Ok(());
        }
        Err(e) => {
            ctx.print_error(OutputError::from(e));
            return Ok(());
        }
    };

    let stale_files = relative_stale_files(ctx, &page.stale_files);
    let covered = page.covered.clone();
    // No live lookup runs on this route, so the index answers for everything
    // its scope covers — including the zeroes.
    let detected = resolve_search_languages(app, language);
    let lower_bounds = ordered_bounds(
        detected.shortfall(ctx),
        index_holes_bound(ctx, &page.unread_paths, &covered),
    );
    // Keep every glob match for an accurate count; finish_symbol_search caps
    // the emitted page at `limit` and sets `truncated`.
    let matches: Vec<SymbolResultOutput> = page
        .rows
        .into_iter()
        .filter(|r| Symbol::path_matches(r.name_path.as_deref().unwrap_or(&r.name), query))
        .map(|r| index_result_output(r, ctx))
        .collect();
    let count = matches.len();

    let shortfall = coverage_shortfall(
        &covered,
        LiveLookup::NotRun {
            requested: &detected.languages,
        },
    );
    // A wildcard is matched against the index alone — no language server can
    // honor `*` — so unlike every other route this zero was never confirmed
    // against disk. With no rows there is no `backend` to read either, which
    // leaves nothing at all to distinguish "no such symbol" from "written
    // since the last build". Said only on the zero: a non-empty answer
    // carries `backend: "index"` on every row, which is the same statement.
    // Said only on the zero, and as a fact of this ROUTE: it is about the index
    // being behind, which is a different axis from the bounds, and passing it
    // through the shared shaping is what keeps the bounds in front of it and
    // keeps its rebuild from surviving a cap that dropped the sentence
    // explaining it.
    let unconfirmed_zero: Vec<(String, String)> = (count == 0)
        .then(|| {
            (
                "Wildcards are matched against the index alone, so a symbol written since the \
                 last build has no row to match"
                    .to_string(),
                "symora search index build".to_string(),
            )
        })
        .into_iter()
        .collect();
    ctx.print_success(with_coverage_disclosure(
        with_emitted_stale(
            finish_symbol_search(
                matches,
                count,
                query,
                language,
                kind,
                limit,
                app.test_scope(),
                &shortfall,
            ),
            &stale_files,
        ),
        &shortfall,
        query,
        DisclosureRoute::IndexConsulted,
        &lower_bounds,
        &unconfirmed_zero,
    ));
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
        // Index rows are extracted exactly from source — never a decoded guess.
        degraded_column: None,
        container: row.container,
        backend: Some("index".to_string()),
        score: row.score,
    }
}

/// What routed a search to live workspace symbols, and what that route owes
/// the answer: the walk that chose the languages to fan out over — which is
/// also what reported the shortfall those languages are short by — and what an
/// index this run could not consult costs.
struct WorkspaceRoute<'a> {
    route: WorkspaceSearchRoute,
    detected: &'a DetectedLanguages,
    index_unavailable: Option<(String, String)>,
}

async fn execute_workspace_symbol_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    routed: WorkspaceRoute<'_>,
) -> Result<()> {
    let WorkspaceRoute {
        route,
        detected,
        index_unavailable,
    } = routed;
    let ctx = &app.output;
    let languages = &detected.languages;
    if languages.is_empty() {
        ctx.print_error(no_languages_error(ctx, detected, language));
        return Ok(());
    }

    let lookup = collect_workspace_symbol_results(app, query, kind, limit, languages).await;
    // The fan-out overfetches and then cuts, so its rows are the whole live
    // answer only when nothing was cut. Read before the rows are moved.
    let mut live = Contribution {
        found: lookup.total > 0,
        cut_short: lookup.results.len() < lookup.total,
    };
    let mut candidates = lookup.results;
    let mut failures = lookup.failures;
    let mut skipped = lookup.skipped;
    let indexing = lookup.indexing;
    let mut count = lookup.total;
    let mut stale_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut index_consulted = false;
    let mut index_covered: Vec<Language> = Vec::new();
    let mut index_bounds: Vec<LowerBound> = Vec::new();
    let mut lower_bounds: Vec<LowerBound> = Vec::new();

    if looks_like_symbol_path(query) {
        if candidates.len() < limit {
            // Document expansion is part of the same live answer: what it finds
            // is live, and what it had to leave behind leaves the live side cut
            // short just as the fan-out's own cap does.
            let expanded =
                collect_document_path_results(app, query, kind, limit, languages, &candidates)
                    .await;
            live.found |= !expanded.results.is_empty();
            live.cut_short |= expanded.capped;
            // Said on its own, not only through `unmerged_overlap`: rows left
            // behind by the widening's own cap shorten the answer whether or
            // not an index also contributed, and a widening that was cut short
            // before finding anything is exactly the case the overlap
            // predicate cannot see.
            if expanded.capped {
                lower_bounds.push(LowerBound::LiveWideningCapped);
            }
            if expanded.undescribed {
                lower_bounds.push(LowerBound::LiveFileNotDescribed);
            }
            failures.extend(expanded.failures);
            skipped.extend(expanded.skipped);
            candidates = merge_symbol_results(candidates, expanded.results);
        } else {
            // The page filled before the widening ran, so it never ran at all.
            // That is the same fact the cap inside it states — files this
            // answer never opened, holding path matches `count` never counted
            // — and the same thing raises the limit past both.
            live.cut_short = true;
            lower_bounds.push(LowerBound::LiveWideningCapped);
        }
    }

    // Scope the supplement to an EXPLICIT `--lang` only — never to an
    // auto-detected single language (which the user did not request). Same
    // rule and helper as every other index-query path. `None` where the route
    // never asks: a route that skipped the index has no store outcome, and
    // standing in a failure for it would let the route the caller chose be
    // overwritten by one it never met.
    let supplement = match route.supplements_from_index() {
        true => Some(
            app.store
                .search_symbols(
                    query,
                    candidate_budget(limit),
                    kind.map(SymbolKind::parse_or_default),
                    explicit_index_language(language),
                )
                .await,
        ),
        false => None,
    };
    let mut route = route;
    let mut supplement_unavailable = None;
    let page = match supplement {
        // An index this run could not read is not the same as there being
        // nothing to supplement with: the answer rests on the live lookup
        // alone, and that is worth saying rather than reading as one the index
        // confirmed. The route becomes what the store said it was, so the
        // remedy fits the state rather than the query that asked — a path
        // query prescribing a build to an index already rebuilding otherwise.
        Some(Err(error)) => {
            // Except when the store never answered at all, which is the
            // transport failing and is reported as itself.
            let Some(reason) = workspace_route_for(&error) else {
                ctx.print_error(OutputError::from(error));
                return Ok(());
            };
            supplement_unavailable = index_unavailable_disclosure(&error);
            route = reason;
            None
        }
        Some(Ok(page)) => Some(page),
        None => None,
    };
    if let Some(page) = page {
        index_consulted = true;
        index_covered = page.covered.clone();
        // A vouched language is one this answer stopped asking about live, so
        // the index is its authority here exactly as on the plain route — a
        // path-like query does not change what the index's holes cost.
        index_bounds = index_holes_bound(ctx, &page.unread_paths, &index_covered);
        lower_bounds.extend(unmerged_overlap(
            live,
            Contribution {
                found: page.total > 0,
                cut_short: page.rows.len() < page.total,
            },
        ));
        count = count.max(page.total);
        // The merged output contains index rows, so the page's staleness
        // applies to it; the live workspace results are current by nature.
        stale_files = relative_stale_files(ctx, &page.stale_files);
        let index_results: Vec<SymbolResultOutput> = page
            .rows
            .into_iter()
            .map(|r| index_result_output(r, ctx))
            .collect();
        candidates = merge_symbol_results(candidates, index_results);
    }

    let mut lower_bounds = {
        let mut bounds = ordered_bounds(detected.shortfall(ctx), index_bounds);
        bounds.extend(lower_bounds);
        bounds
    };
    lower_bounds.dedup();

    count = count.max(candidates.len());
    let shortfall = coverage_shortfall(
        &index_covered,
        LiveLookup::Ran {
            failures: &failures,
            skipped: &skipped,
        },
    );
    let mut route_facts = Vec::from_iter(supplement_unavailable.or(index_unavailable));
    route_facts.extend(
        unconfirmed_zero_fact(
            app.store.as_ref(),
            count,
            &unconfirmed_by_live_lookup(&index_covered, &failures, &skipped),
        )
        .await,
    );
    let section = with_coverage_disclosure(
        with_emitted_stale(
            finish_symbol_search(
                candidates,
                count,
                query,
                language,
                kind,
                limit,
                app.test_scope(),
                &shortfall,
            ),
            &stale_files,
        )
        // Any emitted language having run timed-out makes the whole
        // list a lower bound — captured when each query ran.
        .with_indexing(indexing),
        &shortfall,
        query,
        if index_consulted {
            DisclosureRoute::IndexConsulted
        } else {
            DisclosureRoute::WorkspaceOnly(route)
        },
        &lower_bounds,
        &route_facts,
    );
    ctx.print_success(section);
    Ok(())
}

/// Final shaping shared by every search path: suppress low-value noise,
/// cap emission at `limit`, derive `truncated` from the exact candidate
/// count — never from limit saturation — and publish what the answer could
/// not cover. The shortfall is a parameter because no route may emit a
/// result without stating it: a language missing from a partial answer is
/// hidden better than one missing from an empty answer.
fn finish_symbol_search(
    mut candidates: Vec<SymbolResultOutput>,
    count: usize,
    query: &str,
    language: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    test_scope: &TestScope,
    shortfall: &[Uncovered],
) -> Section<SymbolResultOutput> {
    sort_symbol_results(&mut candidates, query, test_scope);
    candidates.truncate(limit);

    let truncated = candidates.len() < count;
    let hints = symbol_search_hints(query, language, kind, truncated, candidates.len(), limit);
    let next_commands = symbol_search_next_commands(&candidates, query, language);
    Section::with_total(candidates, count)
        .with_hints(hints)
        .with_next_commands(next_commands)
        .with_coverage_gaps(shortfall.iter().copied().map(CoverageGap::from).collect())
}

/// The single language to scope an index `search_symbols` query to: `Some` only
/// for an explicit, recognized `--lang`. An absent or unknown `--lang` leaves
/// the query unscoped, so `--lang rust` no longer returns indexed symbols from
/// other languages.
fn explicit_index_language(language: Option<&str>) -> Option<Language> {
    let lang = Language::parse_or_default(language?);
    (lang != Language::Unknown).then_some(lang)
}

/// Workspace-symbol fan-out outcome: the ranked results plus each failed
/// language with its error, so an empty result can disclose which
/// languages it does not actually cover instead of swallowing them.
/// `indexing` is present when any answering language's query ran under
/// degraded indexing — the combined result is then a lower bound.
struct WorkspaceSymbolLookup {
    results: Vec<SymbolResultOutput>,
    /// Distinct matches the fan-out found, before the emission cap. The
    /// results are overfetched and then cut, so their length is what this
    /// call chose to carry rather than what it saw; a count taken from it
    /// would report a capped list as a complete enumeration.
    total: usize,
    failures: Vec<(Language, LspError)>,
    /// Languages the fan-out never asked, because enough candidates had
    /// already been collected. Their absence from the result says nothing
    /// about them, so they are disclosed exactly as a failure is.
    skipped: Vec<Language>,
    /// Degradation merged across the queried languages. Every caller reaches
    /// the fan-out only for languages the index does not answer, so the
    /// server is the sole source for all of them and this is a genuine
    /// lower-bound marker rather than warm-up noise.
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
    if languages.is_empty() {
        return WorkspaceSymbolLookup {
            results: Vec::new(),
            total: 0,
            failures,
            skipped: Vec::new(),
            indexing: None,
        };
    }
    // Per-language LSP degradation, recorded as the fan-out runs. The two
    // disclosure markers are derived from it once below — per-language, never a
    // single global authoritativeness flag (which would miss an unindexed
    // language's timeout in a bare query that also spans an indexed one).
    let mut lsp_degradation: Vec<(Language, Option<crate::models::lsp::IndexingDegradation>)> =
        Vec::new();

    let workspace_query = workspace_query_from_pattern(query);
    let overfetch_limit = if looks_like_symbol_path(query) {
        limit
    } else {
        limit.saturating_mul(4)
    };
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    let mut skipped: Vec<Language> = Vec::new();

    for (queried, language) in languages.iter().enumerate() {
        if results.len() >= overfetch_limit.saturating_mul(2) {
            skipped.extend_from_slice(&languages[queried..]);
            break;
        }
        let mut symbols = match app.lsp.workspace_symbols(&workspace_query, *language).await {
            Ok(symbols) => {
                lsp_degradation.push((*language, symbols.indexing));
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
            if !answers_for(&symbol.location.file, languages) {
                continue;
            }
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
    }

    results.sort_by(|a, b| {
        score_workspace_symbol(query, b)
            .partial_cmp(&score_workspace_symbol(query, a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.location.file.cmp(&b.location.file))
            .then_with(|| a.location.line.cmp(&b.location.line))
            .then_with(|| a.location.column.cmp(&b.location.column))
    });

    let total = results.len();
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
                degraded_column: symbol.location.degraded_column,
                container: symbol.container,
                backend: Some("workspace".to_string()),
                score,
            }
        })
        .collect();
    sort_symbol_results(&mut outputs, query, app.test_scope());
    WorkspaceSymbolLookup {
        results: outputs,
        total,
        failures,
        skipped,
        // First marker wins — a single IndexingDegradation variant, so order
        // is immaterial.
        indexing: lsp_degradation.iter().find_map(|(_, d)| *d),
    }
}

/// What one source of an answer brought, in the only two terms the union
/// depends on: whether it found anything at all, and whether the rows it
/// found are all still in hand.
#[derive(Clone, Copy, Debug)]
struct Contribution {
    found: bool,
    cut_short: bool,
}

/// Whether the union of a live answer and an index answer can be counted or
/// only bounded from below.
///
/// A path query reaches both sources for the SAME languages, so unlike every
/// other route their answers can name the same symbol and the union is not
/// the sum. Deduplication settles it whenever both sets are in hand; when
/// either was cut short the overlap is unobservable, and the larger source is
/// then a floor under the union rather than a count of it. A source that
/// found nothing leaves no overlap to miss, and the other's total stands.
fn unmerged_overlap(live: Contribution, index: Contribution) -> Option<LowerBound> {
    (live.found && index.found && (live.cut_short || index.cut_short))
        .then_some(LowerBound::UnmergedOverlap)
}

/// Whether a workspace-symbol row belongs to the fan-out that produced it.
/// A server answers for a workspace, not for the language it was chosen by:
/// a TypeScript server with `allowJs` returns rows from `.js` files, and one
/// asked about JavaScript hands back `.ts` ones. Dropping rows that belong to
/// ANOTHER requested language is what makes the fan-out disjoint from an index
/// answer for that language — without it the two sources can count the same
/// symbol twice.
///
/// A path the extension table does not recognise (a shebang script, an
/// extensionless entry point) is kept. What makes that safe is not that the
/// index never holds such a FILE — a build can — but that it never holds a
/// SYMBOL from one: symbol extraction is per language and `Language::Unknown`
/// has no extractor, so no index row exists to double-count against. Dropping
/// the row would discard a real answer a server gave for a file it serves.
fn answers_for(file: &std::path::Path, languages: &[Language]) -> bool {
    match Language::from_path(file) {
        Language::Unknown => true,
        language => languages.contains(&language),
    }
}

/// The requested languages to ask a language server about.
///
/// Everything outside the index's scope, always. Plus everything inside it
/// when the index matched nothing at all — a miss is the one result a server
/// can still improve on, by naming a symbol written since the last build or
/// one declared outside the indexed tree.
///
/// Escalating only from an empty page is also what keeps the two result sets
/// countable as a sum: an index that contributed no rows has nothing for the
/// live answer to overlap with, and one that did is asked about only the
/// languages it does not hold.
fn languages_needing_live_lookup(
    requested: &[Language],
    covered: &[Language],
    index_matched: bool,
) -> Vec<Language> {
    requested
        .iter()
        .copied()
        .filter(|language| !index_matched || !covered.contains(language))
        .collect()
}

/// The union of two result sets, each match kept once. Ranking and the
/// emission cap happen together in `finish_symbol_search`, so the candidate
/// count stays exact and no route can order an answer differently.
fn merge_symbol_results(
    primary: Vec<SymbolResultOutput>,
    secondary: Vec<SymbolResultOutput>,
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

    merged
}

async fn collect_document_path_results(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
    seed_results: &[SymbolResultOutput],
) -> DocumentExpansion {
    let ctx = &app.output;
    let parsed_kind = kind.map(crate::models::symbol::SymbolKind::parse_or_default);
    let leaf = workspace_query_from_pattern(query);
    if leaf.is_empty() {
        return DocumentExpansion::default();
    }

    let mut candidate_files = Vec::new();
    let mut seen_files = HashSet::new();
    let mut capped = false;
    let mut undescribed = false;

    for result in seed_results {
        let file = app.root().join(&result.file);
        if seen_files.insert(file.clone()) {
            candidate_files.push(file);
        }
    }

    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    if candidate_files.len() < limit {
        let seeds = collect_workspace_symbol_results(app, &leaf, kind, limit * 2, languages).await;
        // The leaf requery is a fan-out like any other, and what it could not
        // ask about is a coverage fact — discarding it here would leave the
        // caller publishing a set of languages it never actually covered.
        failures = seeds.failures;
        skipped = seeds.skipped;
        // The fan-out returns its rows already cut to its own overfetch limit
        // while reporting the total it saw, so seeds can run out here while
        // more existed upstream.
        capped |= seeds.total > seeds.results.len();
        let mut seeds = seeds.results.into_iter();
        for result in seeds.by_ref() {
            let file = app.root().join(&result.file);
            if seen_files.insert(file.clone()) {
                candidate_files.push(file);
                if candidate_files.len() >= limit * 2 {
                    break;
                }
            }
        }
        // Reaching the cap on the last seed omitted nothing. Only a seed left
        // unread does.
        capped |= seeds.next().is_some();
    }

    let mut expanded = Vec::new();
    let mut seen_symbols = HashSet::new();
    for file in candidate_files {
        let symbols = app
            .lsp
            .find_symbols(&file, FindSymbolsOptions::default().with_depth(10))
            .await;
        let mut symbols = match symbols {
            Ok(symbols) => symbols,
            // A file whose symbols the server would not give up is a gap in
            // the language it belongs to, recorded the way every other
            // per-language failure is rather than silently skipped. A file
            // whose extension names no language cannot be one — a gap names a
            // language, and `--lang unknown` is not a command anyone can run —
            // so it shortens the widening instead.
            Err(e) => {
                let language = Language::from_path(&file);
                if languages.contains(&language) {
                    failures.push((language, e));
                } else {
                    undescribed = true;
                }
                continue;
            }
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
                    degraded_column: symbol.location.degraded_column,
                    container: symbol.container,
                    backend: Some("document".to_string()),
                });
            }
        }
    }

    sort_symbol_results(&mut expanded, query, app.test_scope());
    capped |= expanded.len() > limit;
    expanded.truncate(limit);
    DocumentExpansion {
        results: expanded,
        capped,
        undescribed,
        failures,
        skipped,
    }
}

/// What widening a path query into document symbols found, what it could not
/// cover, and whether it had to stop before it was done.
///
/// The expansion is a fan-out of its own: it requeries the leaf across the
/// same languages, so its failures and skips are coverage facts the caller has
/// to publish. `capped` and `undescribed` are narrower and separate: rows left
/// behind by a cap are cured by raising `--limit`, a file no server would
/// describe is not, and only the cap makes the union with an index answer
/// uncountable.
#[derive(Default)]
struct DocumentExpansion {
    results: Vec<SymbolResultOutput>,
    capped: bool,
    undescribed: bool,
    failures: Vec<(Language, LspError)>,
    skipped: Vec<Language>,
}

fn rank_of(result: &SymbolResultOutput, query: &str, test_scope: &TestScope) -> i32 {
    symbol_rank(
        query,
        RankedSymbol {
            name: &result.name,
            name_path: result.name_path.as_deref(),
            kind: &result.kind,
            file: std::path::Path::new(&result.file),
        },
        test_scope,
    )
}

fn sort_symbol_results(results: &mut [SymbolResultOutput], query: &str, test_scope: &TestScope) {
    results.sort_by(|a, b| {
        rank_of(b, query, test_scope)
            .cmp(&rank_of(a, query, test_scope))
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.column.cmp(&b.column))
    });
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
    limit: usize,
) -> Vec<String> {
    symbol_lookup_hints(
        query,
        looks_like_symbol_path(query),
        language.is_none(),
        kind.is_none(),
        truncated,
        result_count,
        limit,
    )
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
    use crate::cli::response::disclosure::{
        CoverageReason, symbol_coverage_hints, symbol_coverage_next_commands,
    };

    /// A path query is the one route whose two sources answer for the same
    /// languages, so their union is not the sum and `count` is exact only
    /// while both sets are still in hand to deduplicate.
    #[test]
    fn an_overlap_is_counted_when_both_sources_are_whole_and_bounded_otherwise() {
        let cut = Contribution {
            found: true,
            cut_short: true,
        };
        let whole = Contribution {
            found: true,
            cut_short: false,
        };
        let nothing = Contribution {
            found: false,
            cut_short: false,
        };

        assert_eq!(
            unmerged_overlap(cut, cut),
            Some(LowerBound::UnmergedOverlap)
        );
        assert_eq!(
            unmerged_overlap(whole, cut),
            Some(LowerBound::UnmergedOverlap),
            "one whole source does not make the other's cut rows observable"
        );
        assert_eq!(
            unmerged_overlap(cut, whole),
            Some(LowerBound::UnmergedOverlap)
        );
        assert_eq!(unmerged_overlap(whole, whole), None);
        assert_eq!(
            unmerged_overlap(cut, nothing),
            None,
            "a source that found nothing leaves no overlap to miss"
        );
        assert_eq!(unmerged_overlap(nothing, cut), None);
    }

    #[test]
    fn symbol_hints_are_empty_for_exact_single_result() {
        let hints = symbol_search_hints("SearchCommand/Content", None, None, false, 1, 10);
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
            degraded_column: None,
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
        let section = finish_symbol_search(
            candidates,
            3,
            "alpha",
            None,
            None,
            2,
            &TestScope::new(),
            &[],
        );

        assert_eq!(section.count, 3);
        assert_eq!(section.showing, 2);
        assert!(section.truncated);
    }

    /// The disjointness the merged `count` rests on. A server chosen for one
    /// language routinely answers for another it also handles, so a row is
    /// kept on the language of its FILE, not on the language of the request
    /// that produced it — otherwise a JavaScript lookup served by the
    /// TypeScript server returns the very rows the index already counted.
    #[test]
    fn a_fan_out_keeps_only_files_of_the_languages_it_asked_about() {
        use std::path::Path;
        let asked = [Language::JavaScript];
        assert!(answers_for(Path::new("src/b.js"), &asked));
        assert!(!answers_for(Path::new("src/a.ts"), &asked));
        // The language of the request never widens what its answer covers.
        assert!(!answers_for(Path::new("src/a.ts"), &[Language::Rust]));
        // A file no language claims is attributable to none of them.
        assert!(
            answers_for(Path::new("bin/deploy"), &asked),
            "an unrecognised path is not in the index either, so keeping it cannot double-count"
        );
        // Both asked for, both kept — the fan-out's own domain is honored.
        let both = [Language::TypeScript, Language::JavaScript];
        assert!(answers_for(Path::new("src/a.ts"), &both));
        assert!(answers_for(Path::new("src/b.js"), &both));
    }

    #[test]
    fn explicit_index_language_scopes_only_a_recognized_lang() {
        // A recognized --lang scopes the index query (no cross-language leak);
        // an absent or unknown --lang leaves it unscoped.
        assert_eq!(explicit_index_language(Some("rust")), Some(Language::Rust));
        assert_eq!(explicit_index_language(Some("lua")), Some(Language::Lua));
        assert_eq!(explicit_index_language(None), None);
        assert_eq!(explicit_index_language(Some("not-a-language")), None);
    }

    /// The shortfall an answer publishes is the shortfall it was built
    /// with — a route cannot emit a result without stating what it could
    /// not cover, and that statement does not depend on the result being
    /// empty: a language missing from a partial answer is hidden better
    /// than one missing from an empty one.
    #[test]
    fn a_partial_answer_publishes_its_shortfall_too() {
        let shortfall = vec![Uncovered {
            language: Language::Go,
            reason: CoverageReason::ServerNotInstalled,
        }];
        let published = vec![CoverageGap {
            language: "go".to_string(),
            reason: "server_not_installed".to_string(),
        }];
        let partial = finish_symbol_search(
            vec![result("foo", "src/a.rs")],
            1,
            "foo",
            None,
            None,
            10,
            &TestScope::new(),
            &shortfall,
        );
        assert_eq!(partial.coverage_gaps, published);

        let empty = finish_symbol_search(
            vec![],
            0,
            "foo",
            None,
            None,
            10,
            &TestScope::new(),
            &shortfall,
        );
        assert_eq!(empty.coverage_gaps, published);

        let complete = finish_symbol_search(
            vec![result("foo", "src/a.rs")],
            1,
            "foo",
            None,
            None,
            10,
            &TestScope::new(),
            &[],
        );
        assert!(complete.coverage_gaps.is_empty());
    }

    #[test]
    fn a_covered_language_that_matched_is_answered_from_the_index() {
        assert!(
            languages_needing_live_lookup(&[Language::Rust], &[Language::Rust, Language::Go], true)
                .is_empty()
        );
        // An uncovered language has no other source, matched or not.
        assert_eq!(
            languages_needing_live_lookup(&[Language::Lua], &[Language::Rust], true),
            vec![Language::Lua]
        );
        // A bare query spanning both narrows to the uncovered half.
        assert_eq!(
            languages_needing_live_lookup(
                &[Language::Rust, Language::Lua, Language::Markdown],
                &[Language::Rust],
                true
            ),
            vec![Language::Lua, Language::Markdown]
        );
        // A build that covered nothing leaves every language to the server.
        assert_eq!(
            languages_needing_live_lookup(&[Language::Rust], &[], true),
            vec![Language::Rust]
        );
    }

    /// A symbol written since the last build is in the tree and not in the
    /// index. If a miss stopped at the index, the search would report the
    /// symbol does not exist.
    #[test]
    fn a_miss_is_carried_to_the_language_server_even_for_a_covered_language() {
        assert_eq!(
            languages_needing_live_lookup(
                &[Language::Rust, Language::Go],
                &[Language::Rust, Language::Go],
                false
            ),
            vec![Language::Rust, Language::Go]
        );
    }

    #[test]
    fn finish_symbol_search_complete_results_are_not_truncated() {
        let candidates = vec![result("alpha", "src/a.rs")];
        let section = finish_symbol_search(
            candidates,
            1,
            "alpha",
            None,
            None,
            10,
            &TestScope::new(),
            &[],
        );

        assert_eq!(section.count, 1);
        assert_eq!(section.showing, 1);
        assert!(!section.truncated);
    }

    fn gaps(failures: &[(Language, LspError)], vouched: &[Language]) -> Vec<Uncovered> {
        coverage_shortfall(
            vouched,
            LiveLookup::Ran {
                failures,
                skipped: &[],
            },
        )
    }

    /// A language the fan-out stopped before reaching is as unanswered as
    /// one whose server failed: the result says nothing about it, so it
    /// says so.
    #[test]
    fn a_language_the_fan_out_never_asked_is_disclosed() {
        let shortfall = coverage_shortfall(
            &[Language::Rust],
            LiveLookup::Ran {
                failures: &[],
                skipped: &[Language::Go, Language::Rust],
            },
        );
        assert_eq!(shortfall.len(), 1);
        assert_eq!(shortfall[0].language, Language::Go);
        assert_eq!(shortfall[0].reason, CoverageReason::NotSearched);

        // Every route words it and offers the remedy — the structured gap
        // and the prose come from the same set, so neither can be alone.
        for route in [
            DisclosureRoute::IndexConsulted,
            DisclosureRoute::WorkspaceOnly(WorkspaceSearchRoute::IndexNotBuilt),
        ] {
            let section = with_coverage_disclosure(
                Section::with_total(Vec::<SymbolResultOutput>::new(), 0),
                &shortfall,
                "common",
                route,
                &[],
                &[],
            );
            assert!(section.hints[0].contains("never searched"), "{route:?}");
            assert_eq!(
                section.next_commands,
                vec!["symora search symbols 'common' --lang go"],
                "{route:?}"
            );
        }
    }

    #[test]
    fn coverage_disclosure_fires_for_an_uncovered_language_with_a_failed_server() {
        let failures = vec![(
            Language::Lua,
            LspError::ServerNotInstalled {
                name: "lua-language-server".to_string(),
                install_hint: "brew install lua-language-server".to_string(),
            },
        )];
        let shortfall = gaps(&failures, &[Language::Rust]);

        assert_eq!(shortfall.len(), 1);
        assert_eq!(shortfall[0].language, Language::Lua);
        assert_eq!(shortfall[0].reason, CoverageReason::ServerNotInstalled);

        let hints = symbol_coverage_hints(&shortfall, DisclosureRoute::IndexConsulted);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("lua"));
        assert!(hints[0].contains("server_not_installed"));
        assert_eq!(
            symbol_coverage_next_commands("q", &shortfall, DisclosureRoute::IndexConsulted),
            vec!["symora search content 'q' --lang lua", "symora doctor lua"]
        );
    }

    #[test]
    fn coverage_disclosure_silent_for_a_language_the_index_answered_for() {
        // The index answered for Rust in this very call: a failed server
        // does not make its zero non-exhaustive, so nothing is disclosed.
        let failures = vec![(
            Language::Rust,
            LspError::ServerNotInstalled {
                name: "rust-analyzer".to_string(),
                install_hint: "rustup component add rust-analyzer".to_string(),
            },
        )];
        let shortfall = gaps(&failures, &[Language::Rust]);
        assert!(shortfall.is_empty());
        assert!(symbol_coverage_hints(&shortfall, DisclosureRoute::IndexConsulted).is_empty());
        assert!(
            symbol_coverage_next_commands("q", &shortfall, DisclosureRoute::IndexConsulted)
                .is_empty()
        );
    }

    /// A language the binary CAN extract but this build did not index is
    /// answered live, so a failed server leaves a gap the index cannot fill.
    #[test]
    fn coverage_disclosure_fires_for_an_extractor_language_outside_the_build_scope() {
        let failures = vec![(
            Language::Go,
            LspError::ServerNotInstalled {
                name: "gopls".to_string(),
                install_hint: "go install golang.org/x/tools/gopls@latest".to_string(),
            },
        )];
        let shortfall = gaps(&failures, &[Language::Rust]);
        assert_eq!(shortfall.len(), 1);
        assert_eq!(shortfall[0].language, Language::Go);
        assert_eq!(
            symbol_coverage_next_commands("q", &shortfall, DisclosureRoute::IndexConsulted),
            vec!["symora search content 'q' --lang go", "symora doctor go"]
        );
    }

    /// A dead server must not turn a covered language into a coverage gap.
    /// The build covers rust, so rust is not missing from the answer's domain;
    /// what the dead server cost is the confirmation that the zero still holds
    /// on disk, which is a currency question and is answered against the tree
    /// (`search_symbols_zero_is_authoritative_until_the_tree_moves`).
    #[test]
    fn a_covered_language_is_never_a_gap_however_its_server_fared() {
        let failures = vec![(
            Language::Rust,
            LspError::ServerNotInstalled {
                name: "rust-analyzer".to_string(),
                install_hint: "rustup component add rust-analyzer".to_string(),
            },
        )];
        assert!(gaps(&failures, &[Language::Rust]).is_empty());
        // A language the build does not cover is genuinely outside the
        // answer's domain, and stays a gap.
        assert_eq!(gaps(&failures, &[]).len(), 1);
    }

    /// What the live lookup left unsettled, which is one half of what decides
    /// whether a zero can be published unqualified. A language it answered for
    /// is settled whatever the index's currency.
    #[test]
    fn only_a_language_the_live_lookup_missed_is_left_unconfirmed() {
        let failures = vec![(Language::Rust, LspError::Timeout("rust".to_string()))];
        assert_eq!(
            unconfirmed_by_live_lookup(&[Language::Rust, Language::Go], &failures, &[]),
            vec![Language::Rust]
        );
        assert_eq!(
            unconfirmed_by_live_lookup(&[Language::Rust], &[], &[Language::Rust]),
            vec![Language::Rust]
        );
        assert!(unconfirmed_by_live_lookup(&[Language::Rust], &[], &[]).is_empty());
        assert!(unconfirmed_by_live_lookup(&[], &failures, &[]).is_empty());
    }

    /// A route that never asks a language server discloses what the index
    /// does not hold — and only that. A language the index holds WAS
    /// searched, so its lack of a match is an answer, not a gap; calling
    /// it `not_indexed` would name a cause that is false and send the
    /// agent to rebuild an index that is already there.
    #[test]
    fn an_index_only_route_discloses_only_what_the_index_lacks() {
        let shortfall = coverage_shortfall(
            &[Language::Rust],
            LiveLookup::NotRun {
                requested: &[Language::Rust, Language::Go],
            },
        );
        assert_eq!(shortfall.len(), 1);
        assert_eq!(shortfall[0].language, Language::Go);
        assert_eq!(shortfall[0].reason, CoverageReason::NotIndexed);
        assert!(
            symbol_coverage_hints(&shortfall, DisclosureRoute::IndexConsulted)[0]
                .contains("wildcard")
        );

        assert!(
            coverage_shortfall(
                &[Language::Rust],
                LiveLookup::NotRun {
                    requested: &[Language::Rust],
                },
            )
            .is_empty()
        );
    }

    #[test]
    fn coverage_disclosure_silent_with_no_failures() {
        assert!(gaps(&[], &[Language::Rust]).is_empty());
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
    /// the index answered for rust in this same call, so its zero covers
    /// rust — no coverage claim, no index-build no-op.
    #[test]
    fn index_answered_route_stays_bare_for_a_covered_failure() {
        let failures = vec![server_failure(Language::Rust)];
        let shortfall = gaps(&failures, &[Language::Rust]);
        let section = with_coverage_disclosure(
            Section::with_total(Vec::<SymbolResultOutput>::new(), 0),
            &shortfall,
            "Nonexistent/missing_xyz",
            DisclosureRoute::IndexConsulted,
            &[],
            &[],
        );
        assert!(section.hints.is_empty());
        assert!(section.next_commands.is_empty());
    }

    /// With the index answered, a failure outside what it covered is a
    /// genuine gap and gets the index path's hint/command family — never a
    /// `search index build` that would not cover the language.
    #[test]
    fn index_answered_route_steers_uncovered_failure_to_content_search() {
        let failures = vec![server_failure(Language::Lua)];
        let shortfall = gaps(&failures, &[Language::Rust]);
        let section = with_coverage_disclosure(
            Section::with_total(Vec::<SymbolResultOutput>::new(), 0),
            &shortfall,
            "Mod/handler",
            DisclosureRoute::IndexConsulted,
            &[],
            &[],
        );
        assert_eq!(section.hints.len(), 1);
        assert!(section.hints[0].contains("lua"));
        assert!(section.hints[0].contains("not authoritative for"));
        // The remedy is a LITERAL text search, so the path structure a symbol
        // query carries is stripped from it: `Mod/handler` names a container
        // and what it holds, and no source file contains that spelling.
        assert_eq!(
            section.next_commands,
            vec![
                "symora search content 'handler' --lang lua",
                "symora doctor lua"
            ]
        );
    }

    #[test]
    fn index_not_built_route_steers_extractor_covered_failure_to_index_build() {
        let failures = vec![server_failure(Language::Rust)];
        let section = with_coverage_disclosure(
            Section::with_total(Vec::<SymbolResultOutput>::new(), 0),
            &gaps(&failures, &[]),
            "alpha",
            DisclosureRoute::WorkspaceOnly(WorkspaceSearchRoute::IndexNotBuilt),
            &[],
            &[],
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
        let language = Language::all()
            .into_iter()
            .find(|language| !crate::services::store::SymbolExtractor::is_supported(*language))
            .expect("a language with no extraction query");
        let failures = vec![server_failure(language)];
        let section = with_coverage_disclosure(
            Section::with_total(Vec::<SymbolResultOutput>::new(), 0),
            &gaps(&failures, &[]),
            "alpha",
            DisclosureRoute::WorkspaceOnly(WorkspaceSearchRoute::IndexNotBuilt),
            &[],
            &[],
        );
        assert_eq!(section.hints.len(), 1);
        assert!(section.hints[0].contains(language.lsp_id()));
        assert_eq!(
            section.next_commands,
            vec![
                format!("symora search content 'alpha' --lang {language}"),
                format!("symora doctor {language}"),
            ]
        );
    }

    /// A forced live lookup skipped the index deliberately; the cure is
    /// dropping the flag so the index can answer, not rebuilding it.
    #[test]
    fn forced_route_suggests_dropping_the_flag() {
        let failures = vec![server_failure(Language::Rust)];
        let section = with_coverage_disclosure(
            Section::with_total(Vec::<SymbolResultOutput>::new(), 0),
            &gaps(&failures, &[]),
            "alpha",
            DisclosureRoute::WorkspaceOnly(WorkspaceSearchRoute::Forced),
            &[],
            &[],
        );
        assert_eq!(section.hints.len(), 1);
        assert!(section.hints[0].contains("workspace symbol lookup failed"));
        assert_eq!(
            section.next_commands,
            vec!["symora search symbols 'alpha'", "symora doctor rust"]
        );
    }

    /// A non-empty answer discloses its shortfall too. A missing language
    /// hides better among results than in an empty list: the agent reads
    /// the rows it got as the whole answer.
    #[test]
    fn workspace_failure_disclosure_reaches_non_empty_results() {
        let failures = vec![server_failure(Language::Rust)];
        let section = with_coverage_disclosure(
            Section::with_total(vec![result("alpha", "src/a.rs")], 1),
            &gaps(&failures, &[]),
            "alpha",
            DisclosureRoute::WorkspaceOnly(WorkspaceSearchRoute::IndexNotBuilt),
            &[],
            &[],
        );
        assert_eq!(section.hints.len(), 1);
        assert!(section.hints[0].contains("rust"));
        assert!(!section.next_commands.is_empty());
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
