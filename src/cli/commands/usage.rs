use std::sync::Arc;

use anyhow::Result;
use clap::{Args, ValueEnum};
use futures::future::join_all;
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::ParsedLocation;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::errors::ErrorCode;
use crate::cli::output::OutputContext;
use crate::cli::response::disclosure::{
    LiveLookup, LowerBound, coverage_shortfall, with_lower_bounds,
};
use crate::cli::response::{CoverageGap, Section};
use crate::cli::symbol_discovery::{
    LOW_SIGNAL_KIND_PENALTY, TEST_FILE_PENALTY, broad_symbol_kind_bonus,
    generic_exact_identifier_penalty, is_generic_broad_query, no_languages_error,
    noisy_suffix_penalty, resolve_search_languages, symbol_match_priority,
};
use crate::cli::utils::{AnchorResolution, read_line_at};
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Language;
use crate::models::symbol::Symbol;
use crate::services::TestScope;

/// Maximum concurrent LSP requests (higher = faster but more LSP server load)
const MAX_CONCURRENT_LSP_REQUESTS: usize = 20;

/// Cap on symbols collected across the language fan-out before ranking —
/// bounds work on large polyglot workspaces.
const MAX_USAGE_SYMBOLS: usize = 200;

#[derive(Args, Debug)]
#[command(
    after_long_help = "Use `usage` after you already know the symbol family you care about.\nPrefer `search symbols` first for broad discovery, then run `usage` with a more specific name or --lang.\nYou can also pass a location like `src/main.rs:42` to analyze the symbol at that position.\n"
)]
pub struct UsageArgs {
    /// Search pattern (symbol name or regex)
    pub pattern: String,

    /// Sort results by metric
    #[arg(long, short, default_value = "references")]
    pub sort: SortMetric,

    /// Filter results (comma-separated)
    #[arg(long, short, value_delimiter = ',')]
    pub filter: Option<Vec<UsageFilter>>,

    /// Include metrics in output
    #[arg(long)]
    pub metrics: bool,

    /// Include code snippet
    #[arg(long)]
    pub snippet: bool,

    /// Maximum results to display
    #[arg(long)]
    pub limit: Option<usize>,

    /// Maximum symbols to analyze (for --sort references performance)
    /// Use smaller values for faster results on large codebases
    #[arg(long)]
    pub max_symbols: Option<usize>,

    /// Minimum references required (find important symbols)
    #[arg(long)]
    pub min_refs: Option<usize>,

    /// Language for workspace search (optional; auto-detected if omitted)
    #[arg(long, short)]
    pub lang: Option<String>,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum SortMetric {
    References,
    Name,
}

#[derive(Debug, Clone, ValueEnum, PartialEq)]
pub enum UsageFilter {
    /// Only show symbols that have tests
    HasTests,
    /// Only show symbols that lack tests (for test coverage)
    NoTests,
    /// Only show symbols that have documentation
    HasDocs,
    /// Only show symbols that lack documentation (for doc coverage)
    NoDocs,
    /// Exclude symbols defined in test files
    NotTestFile,
    /// Only show symbols with zero references (dead code detection)
    ZeroRefs,
}

impl UsageFilter {
    /// Whether answering this filter needs facts only a per-symbol probe
    /// produces — a reference set, a hover. One that does not is answered from
    /// the symbol itself, so its route settles `count` before any probe runs
    /// and a probe failure cannot shorten the list. Exhaustive on purpose: a
    /// filter added without an answer here fails to compile rather than
    /// silently taking the route that cannot serve it.
    fn needs_analysis(&self) -> bool {
        match self {
            Self::HasTests | Self::NoTests | Self::HasDocs | Self::NoDocs | Self::ZeroRefs => true,
            Self::NotTestFile => false,
        }
    }
}

impl std::fmt::Display for UsageFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsageFilter::HasTests => write!(f, "has_tests"),
            UsageFilter::NoTests => write!(f, "no_tests"),
            UsageFilter::HasDocs => write!(f, "has_docs"),
            UsageFilter::NoDocs => write!(f, "no_docs"),
            UsageFilter::NotTestFile => write!(f, "not_test_file"),
            UsageFilter::ZeroRefs => write!(f, "zero_refs"),
        }
    }
}

/// Outcome of fanning a workspace-symbol query across the detected
/// languages. `usage` auto-detects multiple languages by file count, so
/// partial coverage is the normal case. `failures` pairs each failed
/// language with its error, `skipped` lists languages left unqueried once
/// enough candidates were collected, and `answered` records whether any
/// server responded at all — together they let the output disclose every
/// coverage gap instead of presenting one as a clean zero.
struct UsageLookup {
    symbols: Vec<Symbol>,
    failures: Vec<(Language, LspError)>,
    skipped: Vec<Language>,
    answered: bool,
    /// Degraded-indexing marker from the first answering language whose
    /// workspace-symbol query ran under a warming index — the combined result
    /// is then a lower bound, disclosed via the section's `indexing` field
    /// rather than presented as a complete enumeration (invariant 4).
    indexing: Option<crate::models::lsp::IndexingDegradation>,
}

async fn collect_usage_symbols(app: &App, pattern: &str, languages: &[Language]) -> UsageLookup {
    let mut symbols = Vec::new();
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let mut answered = false;
    let mut indexing = None;
    for (i, language) in languages.iter().enumerate() {
        // Stop fanning out (and booting more servers) once there are enough
        // candidates to rank — but record the unsearched languages so the gap
        // is disclosed rather than hidden behind the count.
        if symbols.len() >= MAX_USAGE_SYMBOLS {
            skipped.extend_from_slice(&languages[i..]);
            break;
        }
        match app.lsp.workspace_symbols(pattern, *language).await {
            Ok(batch) => {
                answered = true;
                // First answering language's degradation wins (one marker, one
                // variant); `.or` keeps it once set, captures it when still None.
                indexing = indexing.or(batch.indexing);
                symbols.extend(batch.data);
            }
            Err(e) => failures.push((*language, e)),
        }
    }
    UsageLookup {
        symbols: dedupe_usage_symbols(symbols),
        failures,
        skipped,
        answered,
        indexing,
    }
}

/// Pick the failure most worth surfacing when no server could answer. A
/// missing server is the most actionable (the agent can install it), so it
/// wins over transient transport errors.
fn representative_failure(failures: Vec<(Language, LspError)>) -> Option<LspError> {
    let mut errors: Vec<LspError> = failures.into_iter().map(|(_, err)| err).collect();
    if let Some(pos) = errors
        .iter()
        .position(|e| matches!(e, LspError::ServerNotInstalled { .. }))
    {
        return Some(errors.swap_remove(pos));
    }
    errors.into_iter().next()
}

/// Every language absent from the result — one whose server failed, or one
/// left unsearched after enough candidates were found — with its reason.
/// `usage` holds no index of its own, so nothing is vouched for ahead of
/// the fan-out and the shortfall is exactly what the fan-out could not
/// answer.
fn coverage_gaps(failures: &[(Language, LspError)], skipped: &[Language]) -> Vec<CoverageGap> {
    coverage_shortfall(&[], LiveLookup::Ran { failures, skipped })
        .into_iter()
        .map(CoverageGap::from)
        .collect()
}

fn dedupe_usage_symbols(symbols: Vec<Symbol>) -> Vec<Symbol> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for symbol in symbols {
        let key = format!(
            "{}:{}:{}:{}",
            symbol.location.file.display(),
            symbol.location.line,
            symbol.location.column,
            symbol.path()
        );
        if seen.insert(key) {
            deduped.push(symbol);
        }
    }
    deduped
}

fn rank_usage_symbols(symbols: &mut [Symbol], query: &str, test_scope: &TestScope) {
    symbols.sort_by(|a, b| {
        usage_symbol_priority(b, query, test_scope)
            .cmp(&usage_symbol_priority(a, query, test_scope))
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.location.file.cmp(&b.location.file))
            .then_with(|| a.location.line.cmp(&b.location.line))
            .then_with(|| a.location.column.cmp(&b.location.column))
    });
}

fn usage_symbol_priority(symbol: &Symbol, query: &str, test_scope: &TestScope) -> i32 {
    let name = symbol.name.to_ascii_lowercase();
    let path = symbol.path().to_ascii_lowercase();
    let kind = symbol.kind.to_string();
    let match_priority = symbol_match_priority(query, &name, &path);
    let test_penalty = if test_scope.is_test_file(&symbol.location.file) {
        TEST_FILE_PENALTY
    } else {
        0
    };
    let kind_penalty = if symbol.kind.is_low_level() {
        LOW_SIGNAL_KIND_PENALTY
    } else {
        0
    };
    let suffix_penalty = noisy_suffix_penalty(&name, &query.to_ascii_lowercase());
    let generic_exact_penalty =
        generic_exact_identifier_penalty(query, &name, &kind, symbol.kind.is_low_level());
    let kind_bonus = broad_symbol_kind_bonus(query, &name, &kind, symbol.kind.is_low_level());

    match_priority + kind_bonus
        - test_penalty
        - kind_penalty
        - suffix_penalty
        - generic_exact_penalty
}

/// Hints about how the query was ASKED. What the analysis could not reach is a
/// lower bound rather than advice, and `LowerBound::AnalysisCapped` states it
/// once — including on the empty answer, which needs it most.
fn usage_hints(query: &str, auto_lang: bool) -> Vec<String> {
    let mut hints = Vec::new();
    if is_generic_broad_query(query) {
        hints.push(
            "This query is broad; prefer a more specific symbol name or add --lang first"
                .to_string(),
        );
    }
    if auto_lang {
        hints.push(
            "Add --lang for faster and more targeted usage analysis in large workspaces"
                .to_string(),
        );
    }
    hints.truncate(3);
    hints
}

fn usage_hints_for_empty(query: &str, auto_lang: bool, resolved_from: Option<&str>) -> Vec<String> {
    let mut hints = usage_hints(query, auto_lang);
    if let Some(loc) = resolved_from {
        hints.push(format!(
            "Workspace symbol lookup was empty for the resolved symbol. Continue with `symora symbols {}` or `symora refs {}` for file-level follow-up.",
            loc.split(':').next().unwrap_or(loc),
            loc
        ));
    }
    hints.truncate(3);
    hints
}

#[derive(Debug, Serialize)]
pub struct UsageOutput {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_from: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters_applied: Vec<String>,
    /// If set, indicates analysis was truncated at this many symbols
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyzed: Option<usize>,
    /// Languages absent from this result — a server that failed, or one left
    /// unsearched after enough candidates were found. Present whenever
    /// coverage is partial, so the reported `count` is never mistaken for an
    /// exhaustive enumeration — the same lower-bound honesty the `indexing`
    /// marker provides.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub coverage_gaps: Vec<CoverageGap>,
    #[serde(flatten)]
    pub section: Section<UsageResult>,
}

#[derive(Debug, Serialize)]
pub struct UsageResult {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<UsageMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageMetrics {
    pub references: usize,
    pub has_tests: bool,
    /// Omitted when the hover that would settle it failed. A hover that
    /// ANSWERS with nothing means undocumented; one that fails never reached
    /// that verdict, and `false` would publish it anyway.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_docs: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_files: Vec<String>,
}

pub async fn execute(args: UsageArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let limit = args.limit.unwrap_or(10);
    let max_symbols = args.max_symbols.unwrap_or(50);
    let resolved = match resolve_usage_query(app, &args.pattern, args.lang.as_deref()).await {
        Ok(resolved) => resolved,
        Err(error) => {
            match error.downcast::<OutputError>() {
                Ok(error) => ctx.print_error(error),
                Err(error) => match error.downcast::<LspError>() {
                    Ok(error) => ctx.print_error(error),
                    Err(error) => return Err(error),
                },
            }
            return Ok(());
        }
    };

    let detected = resolve_search_languages(app, resolved.language_override.as_deref());
    if detected.languages.is_empty() {
        ctx.print_error(no_languages_error(
            ctx,
            &detected,
            resolved.language_override.as_deref(),
        ));
        return Ok(());
    }
    let languages = detected.languages.clone();

    // A language hidden behind an unreadable path never reaches
    // `coverage_gaps`, so the walk's shortfall is what makes the answer read
    // as a lower bound — computed before the first exit, because the empty
    // answer is the one most likely to be misread as "no usages".
    let bound = detected.shortfall(ctx);

    let UsageLookup {
        mut symbols,
        failures,
        skipped,
        answered,
        indexing,
    } = collect_usage_symbols(app, &resolved.query, &languages).await;
    // Coverage gaps — disclosed on every result, empty or not, so partial
    // coverage is always visible rather than hidden behind a count.
    let gaps = coverage_gaps(&failures, &skipped);
    rank_usage_symbols(&mut symbols, &resolved.query, app.test_scope());

    if symbols.is_empty() {
        // No server could answer at all: that failure is the result, not an
        // empty list that would read as a definitive "no usages". An
        // unanswered fan-out always carries at least one failure to surface.
        if !answered {
            if let Some(err) = representative_failure(failures) {
                ctx.print_error(err);
            }
            return Ok(());
        }
        // Some languages were searched and genuinely found nothing; any that
        // failed or were skipped ride along in `coverage_gaps`.
        let resolved_from = resolved.resolved_from.clone();
        ctx.print_success(UsageOutput {
            query: resolved.query.clone(),
            resolved_from: resolved_from.clone(),
            filters_applied: vec![],
            analyzed: None,
            coverage_gaps: gaps,
            section: with_lower_bounds(
                Section::new(vec![])
                    .with_hints(usage_hints_for_empty(
                        &resolved.query,
                        resolved.language_override.is_none(),
                        resolved_from.as_deref(),
                    ))
                    .with_indexing(indexing),
                &Vec::from_iter(bound),
            ),
        });
        return Ok(());
    }

    let filters = args.filter.as_deref().unwrap_or_default();
    let filter_names: Vec<String> = filters.iter().map(|f| f.to_string()).collect();
    let test_scope = app.test_scope();

    // Apply pre-filter for NotTestFile (no LSP calls needed)
    if filters.contains(&UsageFilter::NotTestFile) {
        symbols.retain(|s| !test_scope.is_test_file(&s.location.file));
    }

    // Fast path: sort by name without LSP calls for references
    // Only fetch references for limited results
    let needs_refs_for_sort = matches!(args.sort, SortMetric::References);
    let needs_refs_for_filter =
        filters.iter().any(|filter| filter.needs_analysis()) || args.min_refs.is_some();
    let needs_refs = needs_refs_for_sort || needs_refs_for_filter || args.metrics;

    let (items, count, analyzed, ref_indexing, unanalysed) = if !needs_refs {
        // Fast path: no LSP reference calls needed
        let mut sorted_symbols = symbols;
        sorted_symbols.sort_by(|a, b| a.name.cmp(&b.name));

        let limited: Vec<UsageResult> = sorted_symbols
            .iter()
            .take(limit)
            .map(|symbol| build_result_without_refs(symbol, ctx.root(), args.snippet))
            .collect();

        let total = sorted_symbols.len();
        (limited, total, None, None, 0)
    } else if !needs_refs_for_sort && !needs_refs_for_filter {
        // Medium path: sort by name first, then fetch refs only for limited results
        let mut sorted_symbols = symbols;
        sorted_symbols.sort_by(|a, b| a.name.cmp(&b.name));

        let limited_symbols: Vec<_> = sorted_symbols.iter().take(limit).collect();
        let total = sorted_symbols.len();

        // No filter here reads an analysis — that is what routed the call to
        // this path — so none is passed, and the probe has nothing to reject.
        // `total` is then the symbol count, settled before any probe ran.
        let (analysed, ref_indexing) =
            fetch_refs_parallel(app, &limited_symbols, ctx.root(), &args, &[], test_scope).await;
        // A probe that answered nothing costs this symbol its metrics, and
        // dropping the symbol to hide that would turn a missing field into a
        // missing row — against a count already settled. It is emitted as the
        // fast path emits every symbol, without metrics, and the answer stays
        // exactly as long as its count says.
        let results: Vec<UsageResult> = analysed
            .into_iter()
            .zip(&limited_symbols)
            .map(|(analysed, symbol)| match analysed {
                Analysed::Kept(result) => *result,
                Analysed::Failed | Analysed::FilteredOut => {
                    build_result_without_refs(symbol, ctx.root(), args.snippet)
                }
            })
            .collect();
        (results, total, None, ref_indexing, 0)
    } else {
        // Slow path: need references for sorting or filtering
        // Limit symbols to analyze for performance (each requires LSP call)
        let symbols_to_process: Vec<_> = symbols.iter().take(max_symbols).collect();
        let analyzed = if symbols.len() > max_symbols {
            Some(max_symbols)
        } else {
            None
        };

        let (analysed, ref_indexing) = fetch_refs_parallel(
            app,
            &symbols_to_process,
            ctx.root(),
            &args,
            filters,
            test_scope,
        )
        .await;
        // This path sorts or filters on reference counts, so a symbol whose
        // analysis failed has no answer to give and cannot be emitted without
        // inventing one. It leaves the count, which is what makes the count a
        // lower bound here and not on the path above.
        let unanalysed = analysed
            .iter()
            .filter(|a| matches!(a, Analysed::Failed))
            .count();
        let all_results: Vec<UsageResult> = analysed
            .into_iter()
            .filter_map(|a| match a {
                Analysed::Kept(result) => Some(*result),
                Analysed::Failed | Analysed::FilteredOut => None,
            })
            .collect();

        let total = all_results.len();
        let mut with_refs: Vec<_> = all_results
            .into_iter()
            .map(|r| {
                let refs = r.metrics.as_ref().map(|m| m.references).unwrap_or(0);
                (r, refs)
            })
            .collect();

        match args.sort {
            SortMetric::References => with_refs.sort_by_key(|item| std::cmp::Reverse(item.1)),
            SortMetric::Name => with_refs.sort_by(|a, b| a.0.name.cmp(&b.0.name)),
        }

        // Strip metrics if user didn't request them (they were only used for sorting/filtering)
        let limited: Vec<UsageResult> = with_refs
            .into_iter()
            .take(limit)
            .map(|(mut r, _)| {
                if !args.metrics {
                    r.metrics = None;
                }
                r
            })
            .collect();

        (limited, total, analyzed, ref_indexing, unanalysed)
    };

    // Merge workspace-symbol degradation (from the symbol collection) with any
    // reference-query degradation (from the per-symbol ref counts): either makes
    // the result a lower bound, and both are the same single marker on output.
    let indexing = indexing.or(ref_indexing);

    // A symbol whose analysis failed answered none of the filters and is
    // absent from the list because of that, not because it did not match.
    let bounds: Vec<LowerBound> = bound
        .into_iter()
        .chain((unanalysed > 0).then_some(LowerBound::SymbolsNotAnalysed(unanalysed)))
        // The cap is documented and configurable, but unlike `--limit` it
        // stops the analysis rather than the emission: the symbols past it
        // never reached a filter, so they are absent from `count` and not
        // merely from this page.
        .chain(analyzed.map(LowerBound::AnalysisCapped))
        .collect();
    let hints = usage_hints(&resolved.query, resolved.language_override.is_none());

    let response = UsageOutput {
        query: resolved.query,
        resolved_from: resolved.resolved_from,
        filters_applied: filter_names,
        analyzed,
        coverage_gaps: gaps,
        section: with_lower_bounds(
            Section::with_total(items, count)
                .with_hints(hints)
                .with_indexing(indexing),
            &bounds,
        ),
    };

    ctx.print_success(response);

    Ok(())
}

struct ResolvedUsageQuery {
    query: String,
    language_override: Option<String>,
    resolved_from: Option<String>,
}

async fn resolve_usage_query(
    app: &App,
    input: &str,
    lang: Option<&str>,
) -> Result<ResolvedUsageQuery> {
    if !ParsedLocation::is_location_format(input) {
        return Ok(ResolvedUsageQuery {
            query: input.to_string(),
            language_override: lang.map(str::to_string),
            resolved_from: None,
        });
    }

    let loc = ParsedLocation::parse(input)?.to_absolute()?;
    // The shared anchor rules: an omitted column targets the symbol DECLARED
    // on the line (first declaration on ambiguity — the resolved name is
    // echoed via `resolved_from`/`query`), a column the token at that
    // position, resolved through its definition when it is a usage.
    let anchor = crate::cli::analysis::resolve_anchor(
        app.lsp.as_ref(),
        &loc,
        FindSymbolsOptions::default().with_depth(10),
    )
    .await?;
    let position = if loc.column_explicit {
        format!("{}:{}:{}", ctx_rel(app, &loc.file), loc.line, loc.column)
    } else {
        format!("{}:{}", ctx_rel(app, &loc.file), loc.line)
    };
    let Some(symbol) = anchor.symbol else {
        // `usage` analyzes symbols by name across the workspace; a position
        // that denotes an unlisted binding, or nothing, has no such name — say
        // so, rather than searching for the position string.
        let error = match anchor.resolution {
            AnchorResolution::Binding => OutputError::invalid(format!(
                "{position} denotes a binding the symbol tree does not list (a local, a \
                 parameter, a module, or a generated item), which usage does not analyze"
            ))
            .with_hint(format!("Use `symora refs {position}` for its references")),
            // The read failed, so nothing was learned about the position.
            // "No symbol" is a conclusion about the tree, and answering an
            // I/O failure with one sends a caller off to look elsewhere for
            // a symbol that may well be there.
            AnchorResolution::Unavailable => OutputError::new(
                ErrorCode::Io,
                format!("{position} could not be read to resolve a symbol"),
            )
            .with_hint("Retry, or anchor at a declaration (e.g. a `symora search symbols` result)"),
            AnchorResolution::Resolved | AnchorResolution::NotASymbol => {
                if loc.column_explicit {
                    OutputError::not_found(format!("No symbol at {position}")).with_hint(format!(
                        "Address the symbol declared on or enclosing the line with {}:{}, or a \
                         declaration from `symora symbols`",
                        ctx_rel(app, &loc.file),
                        loc.line
                    ))
                } else {
                    OutputError::not_found(format!("No symbol at {position}")).with_hint(
                        "Anchor at a declaration (e.g. a `symora search symbols` result)",
                    )
                }
            }
        };
        return Err(error.into());
    };

    let inferred_lang = Language::from_path(&anchor.file);
    Ok(ResolvedUsageQuery {
        query: symbol.name.clone(),
        language_override: lang.map(str::to_string).or_else(|| {
            (inferred_lang != Language::Unknown).then(|| inferred_lang.lsp_id().to_string())
        }),
        resolved_from: Some(position),
    })
}

fn ctx_rel(app: &App, file: &std::path::Path) -> String {
    app.output.relative_path(file)
}

fn build_result_without_refs(
    symbol: &Symbol,
    root: &std::path::Path,
    with_snippet: bool,
) -> UsageResult {
    let signature = crate::cli::utils::extract_signature(symbol.body.as_deref());
    let snippet = if with_snippet {
        // workspace_symbols doesn't include body, so read from file
        symbol
            .body
            .clone()
            .or_else(|| read_line_at(&symbol.location.file, symbol.location.line).ok())
    } else {
        None
    };

    UsageResult {
        name: symbol.name.clone(),
        file: OutputContext::format_path(&symbol.location.file, root),
        line: symbol.location.line,
        kind: symbol.kind.to_string(),
        signature,
        metrics: None,
        snippet,
    }
}

async fn fetch_refs_parallel(
    app: &App,
    symbols: &[&Symbol],
    root: &std::path::Path,
    args: &UsageArgs,
    filters: &[UsageFilter],
    test_scope: &TestScope,
) -> (
    Vec<Analysed>,
    Option<crate::models::lsp::IndexingDegradation>,
) {
    // Use semaphore for fine-grained concurrency control
    // This is faster than batch processing because it keeps MAX_CONCURRENT requests in flight
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_LSP_REQUESTS));

    // Launch all futures immediately (semaphore controls actual concurrency)
    let futures: Vec<_> = symbols
        .iter()
        .map(|symbol| {
            let sem = Arc::clone(&semaphore);
            async move {
                let Ok(_permit) = sem.acquire().await else {
                    // The semaphore closed, so this symbol was never analysed
                    // at all — the same unknown as a probe that failed.
                    return (Analysed::Failed, None);
                };
                fetch_single_symbol_refs(app, symbol, root, args, filters, test_scope).await
            }
        })
        .collect();

    let results = join_all(futures).await;

    // Merge the find_references degradation: any degraded reference query makes
    // the count (and the default --sort references order / min_refs / ZeroRefs
    // filters) a lower bound, surfaced once on the section so the answer is
    // never presented as complete. The per-symbol verdicts stay in the order
    // they were asked for, so a caller can pair one with the symbol it is for.
    let mut analysed = Vec::with_capacity(symbols.len());
    let mut indexing = None;
    for (result, idx) in results {
        // If ANY reference query degraded, the merged count is a lower bound;
        // `.or` keeps the first marker seen (one variant, so order is moot).
        indexing = indexing.or(idx);
        analysed.push(result);
    }
    (analysed, indexing)
}

async fn fetch_single_symbol_refs(
    app: &App,
    symbol: &Symbol,
    root: &std::path::Path,
    args: &UsageArgs,
    filters: &[UsageFilter],
    test_scope: &TestScope,
) -> (Analysed, Option<crate::models::lsp::IndexingDegradation>) {
    let analysis = LocationAnalysis::for_symbol(
        app.lsp.as_ref(),
        &symbol.location.file,
        symbol.clone(),
        root,
    )
    .await;
    // A degraded find_references makes the reference COUNT a lower bound — and
    // the count drives the default `--sort references` ordering and the
    // min_refs/ZeroRefs filters — so report it for disclosure even when this
    // symbol is then filtered out, exactly as `callers.rs` threads the marker.
    let indexing = analysis.as_ref().ok().and_then(LocationAnalysis::indexing);
    let Ok(analysis) = analysis.as_ref() else {
        // Every filter below reads a reference count, and an analysis that
        // failed has none — not zero. Letting it default would put an
        // unanalysed symbol in a `zero-refs` list, which is a list an agent
        // deletes from, and keep it out of a `min-refs` one. The symbol drops
        // out and the answer says it is short.
        return (Analysed::Failed, indexing);
    };
    let classified = analysis.classify(test_scope);

    let ref_count = classified.total;
    let has_tests = classified.test > 0;

    if filters.contains(&UsageFilter::HasTests) && !has_tests {
        return (Analysed::FilteredOut, indexing);
    }

    // Filter: only symbols without tests (for test coverage analysis)
    if filters.contains(&UsageFilter::NoTests) && has_tests {
        return (Analysed::FilteredOut, indexing);
    }

    // Filter: only symbols with zero references (dead code detection)
    if filters.contains(&UsageFilter::ZeroRefs) && ref_count > 0 {
        return (Analysed::FilteredOut, indexing);
    }

    // Filter: only symbols with at least N references (find important symbols)
    if let Some(min) = args.min_refs
        && ref_count < min
    {
        return (Analysed::FilteredOut, indexing);
    }

    let needs_docs_check = args.metrics
        || filters.contains(&UsageFilter::HasDocs)
        || filters.contains(&UsageFilter::NoDocs);

    // A hover that ANSWERS with nothing means undocumented; one that fails
    // leaves it unknown, which is a third state and not the second.
    let has_docs = if needs_docs_check {
        app.lsp
            .hover(
                &symbol.location.file,
                symbol.location.line,
                symbol.location.column,
            )
            .await
            .ok()
            .map(|hover| hover.is_some_and(|h| !h.content.is_empty()))
    } else {
        None
    };

    if let Some(outcome) = docs_shortfall(has_docs, filters) {
        return (outcome, indexing);
    }

    let metrics = if args.metrics {
        // Only collect up to 3 test files (avoid allocating entire list)
        let test_files: Vec<String> = classified
            .test_refs
            .iter()
            .map(|r| OutputContext::format_path(&r.file, root))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .take(3)
            .collect();

        Some(UsageMetrics {
            references: ref_count,
            has_tests,
            has_docs,
            test_files,
        })
    } else {
        // Still need reference count for sorting
        Some(UsageMetrics {
            references: ref_count,
            has_tests,
            has_docs,
            test_files: vec![],
        })
    };

    let snippet = if args.snippet {
        // workspace_symbols doesn't include body, so read from file
        symbol
            .body
            .clone()
            .or_else(|| read_line_at(&symbol.location.file, symbol.location.line).ok())
    } else {
        None
    };

    let signature = crate::cli::utils::extract_signature(symbol.body.as_deref());

    (
        Analysed::Kept(Box::new(UsageResult {
            name: symbol.name.clone(),
            file: OutputContext::format_path(&symbol.location.file, root),
            line: symbol.location.line,
            kind: symbol.kind.to_string(),
            signature,
            metrics,
            snippet,
        })),
        indexing,
    )
}

/// What an unknown or unwanted documentation verdict costs a symbol.
///
/// Three states, not two: a hover that ANSWERS with nothing means
/// undocumented, one that fails means unknown. A filter that reads
/// documentation has no answer for the unknown, so the symbol answered none of
/// the filters applied — which is exactly what a failed analysis reports, and
/// why the count then says it is short. A `--metrics` run asked for the fact
/// rather than filtering on it, so the unknown costs that run a missing field
/// and not a missing row; the reference count its sort and count are built
/// from was never in doubt.
fn docs_shortfall(has_docs: Option<bool>, filters: &[UsageFilter]) -> Option<Analysed> {
    match has_docs {
        None => filters
            .iter()
            .any(|filter| matches!(filter, UsageFilter::HasDocs | UsageFilter::NoDocs))
            .then_some(Analysed::Failed),
        Some(true) => filters
            .contains(&UsageFilter::NoDocs)
            .then_some(Analysed::FilteredOut),
        Some(false) => filters
            .contains(&UsageFilter::HasDocs)
            .then_some(Analysed::FilteredOut),
    }
}

/// What analysing one symbol produced.
///
/// Every usage filter reads a reference or documentation fact, and a probe
/// that failed has none — not zero, and not "undocumented" — so a failure and
/// a filter miss are different verdicts that both yield no row. Collapsing
/// them would put an unanalysed symbol in a `zero-refs` list, which is a list
/// an agent deletes from, and it would hide from the caller which of its rows
/// are missing because nothing could be learned about them. Only the caller
/// knows what its `count` is derived from, so the two stay distinct until it
/// decides.
enum Analysed {
    Kept(Box<UsageResult>),
    FilteredOut,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hover that could not be read leaves documentation unknown, and
    /// unknown is not "undocumented". A filter that reads it has no answer for
    /// the symbol at all; a `--metrics` run that only wanted the fact keeps the
    /// row and loses the field, because the reference count its count is built
    /// from was never in doubt.
    #[test]
    fn an_unreadable_hover_costs_a_filter_the_symbol_and_metrics_only_the_field() {
        let metrics_only: &[UsageFilter] = &[];
        assert!(
            docs_shortfall(None, metrics_only).is_none(),
            "nothing here filters on documentation, so the row stands"
        );
        assert!(docs_shortfall(Some(true), metrics_only).is_none());
        assert!(docs_shortfall(Some(false), metrics_only).is_none());

        for filter in [UsageFilter::HasDocs, UsageFilter::NoDocs] {
            let name = filter.to_string();
            assert!(
                matches!(docs_shortfall(None, &[filter]), Some(Analysed::Failed)),
                "{name} cannot be answered for a symbol whose hover failed"
            );
        }
        assert!(matches!(
            docs_shortfall(Some(false), &[UsageFilter::HasDocs]),
            Some(Analysed::FilteredOut)
        ));
        assert!(docs_shortfall(Some(true), &[UsageFilter::HasDocs]).is_none());
        assert!(matches!(
            docs_shortfall(Some(true), &[UsageFilter::NoDocs]),
            Some(Analysed::FilteredOut)
        ));
        assert!(docs_shortfall(Some(false), &[UsageFilter::NoDocs]).is_none());

        // A filter that reads something else says nothing about documentation.
        assert!(docs_shortfall(None, &[UsageFilter::HasTests]).is_none());

        // The field goes missing rather than reading `false`, which would
        // publish a verdict the hover never reached.
        let metrics = |has_docs| UsageMetrics {
            references: 1,
            has_tests: false,
            has_docs,
            test_files: vec![],
        };
        let unknown = serde_json::to_value(metrics(None)).unwrap();
        assert!(unknown.get("has_docs").is_none(), "{unknown}");
        assert_eq!(
            serde_json::to_value(metrics(Some(false))).unwrap()["has_docs"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn usage_output_flattens_the_section_contract() {
        let output = UsageOutput {
            query: "AuthUser".to_string(),
            resolved_from: Some("src/main.rs:10:5".to_string()),
            filters_applied: vec![],
            analyzed: Some(10),
            coverage_gaps: vec![],
            section: Section::with_total(
                vec![UsageResult {
                    name: "AuthUser".to_string(),
                    file: "src/main.rs".to_string(),
                    line: 10,
                    kind: "struct".to_string(),
                    signature: None,
                    metrics: None,
                    snippet: None,
                }],
                5,
            )
            .with_hints(vec!["Add --lang".to_string()]),
        };

        let value = serde_json::to_value(output).unwrap();
        assert!(value.get("items").is_some());
        assert!(value.get("results").is_none());
        // Empty coverage list stays absent — agents never parse a noise field.
        assert!(value.get("coverage_gaps").is_none());
        assert_eq!(value["count"], 5);
        assert_eq!(value["showing"], 1);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["query"], "AuthUser");
    }

    #[test]
    fn usage_output_discloses_coverage_gaps_when_coverage_is_partial() {
        let output = UsageOutput {
            query: "Foo".to_string(),
            resolved_from: None,
            filters_applied: vec![],
            analyzed: None,
            coverage_gaps: vec![CoverageGap {
                language: "rust".to_string(),
                reason: "server_not_installed".to_string(),
            }],
            section: Section::new(Vec::<UsageResult>::new()),
        };
        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["coverage_gaps"][0]["language"], "rust");
        assert_eq!(value["coverage_gaps"][0]["reason"], "server_not_installed");
    }

    #[test]
    fn usage_discloses_degraded_workspace_indexing() {
        // A workspace-symbol query that ran under a warming index makes the
        // result a lower bound; usage must surface that via `indexing` rather
        // than present the partial list as a complete enumeration (invariant 4).
        let output = UsageOutput {
            query: "Foo".to_string(),
            resolved_from: None,
            filters_applied: vec![],
            analyzed: None,
            coverage_gaps: vec![],
            section: Section::new(Vec::<UsageResult>::new())
                .with_indexing(Some(crate::models::lsp::IndexingDegradation::TimedOut)),
        };
        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["indexing"], "timed_out");
    }

    #[test]
    fn usage_empty_hints_include_file_level_followup_for_resolved_locations() {
        let hints = usage_hints_for_empty("root", true, Some("src/main.py:54:11"));
        assert!(hints.iter().any(|h| h.contains("file-level follow-up")));
    }

    #[test]
    fn representative_failure_prefers_missing_server_over_transport_error() {
        let failures = vec![
            (Language::Go, LspError::Timeout("slow".to_string())),
            (
                Language::Rust,
                LspError::ServerNotInstalled {
                    name: "rust-analyzer".to_string(),
                    install_hint: "rustup component add rust-analyzer".to_string(),
                },
            ),
        ];
        let picked = representative_failure(failures).expect("a failure to surface");
        assert!(matches!(picked, LspError::ServerNotInstalled { .. }));
    }

    #[test]
    fn representative_failure_is_none_when_every_server_answered() {
        assert!(representative_failure(vec![]).is_none());
    }

    #[test]
    fn coverage_gaps_disclose_failures_and_skips_sorted_with_reasons() {
        let failures = vec![
            (
                Language::Rust,
                LspError::ServerNotInstalled {
                    name: "rust-analyzer".to_string(),
                    install_hint: "x".to_string(),
                },
            ),
            (Language::Go, LspError::Timeout("slow".to_string())),
        ];
        let skipped = vec![Language::Python];
        let gaps = coverage_gaps(&failures, &skipped);

        let by_lang = |lang: &str| {
            gaps.iter()
                .find(|g| g.language == lang)
                .map(|g| g.reason.as_str())
        };
        assert_eq!(by_lang("rust"), Some("server_not_installed"));
        assert_eq!(by_lang("go"), Some("timed_out"));
        assert_eq!(by_lang("python"), Some("not_searched"));
        // Sorted by language for deterministic output.
        let langs: Vec<&str> = gaps.iter().map(|g| g.language.as_str()).collect();
        let mut sorted = langs.clone();
        sorted.sort_unstable();
        assert_eq!(langs, sorted);
    }
}
