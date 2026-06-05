use std::sync::Arc;

use anyhow::Result;
use clap::{Args, ValueEnum};
use futures::future::join_all;
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::ParsedLocation;
use crate::cli::output::OutputContext;
use crate::cli::response::Section;
use crate::cli::symbol_discovery::{
    broad_symbol_kind_bonus, detect_languages_by_file_count, generic_exact_identifier_penalty,
    is_probably_test_path, noisy_suffix_penalty, symbol_match_priority,
};
use crate::cli::utils::{TestMatcher, find_symbol_at_position, read_line_at};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Language;
use crate::models::symbol::Symbol;

/// Maximum concurrent LSP requests (higher = faster but more LSP server load)
const MAX_CONCURRENT_LSP_REQUESTS: usize = 20;

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

fn resolve_usage_languages(app: &App, lang: Option<&str>) -> Vec<Language> {
    match lang.map(Language::parse_or_default) {
        Some(Language::Unknown) => vec![],
        Some(language) => vec![language],
        None => detect_languages_by_file_count(app.root(), &Language::all()),
    }
}

async fn collect_usage_symbols(app: &App, pattern: &str, languages: &[Language]) -> Vec<Symbol> {
    let mut all = Vec::new();
    for language in languages {
        let Ok(batch) = app.lsp.workspace_symbols(pattern, *language).await else {
            continue;
        };
        all.extend(batch);
        if all.len() >= 200 {
            break;
        }
    }
    dedupe_usage_symbols(all)
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

fn rank_usage_symbols(symbols: &mut [Symbol], query: &str) {
    symbols.sort_by(|a, b| {
        usage_symbol_priority(b, query)
            .cmp(&usage_symbol_priority(a, query))
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.location.file.cmp(&b.location.file))
            .then_with(|| a.location.line.cmp(&b.location.line))
    });
}

fn usage_symbol_priority(symbol: &Symbol, query: &str) -> i32 {
    let name = symbol.name.to_ascii_lowercase();
    let path = symbol.path().to_ascii_lowercase();
    let kind = symbol.kind.to_string();
    let match_priority = symbol_match_priority(query, &name, &path);
    let file = symbol.location.file.display().to_string();
    let test_penalty = if is_probably_test_path(&file) { 8 } else { 0 };
    let kind_penalty = if symbol.kind.is_low_level() { 6 } else { 0 };
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

fn usage_hints(query: &str, auto_lang: bool, showing: usize, truncated: bool) -> Vec<String> {
    let mut hints = Vec::new();
    if query.len() <= 8 && query.chars().all(|c| c.is_ascii_lowercase()) {
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
    if truncated && showing > 0 {
        hints.push(
            "Increase --max-symbols when important candidates may be missing from analysis"
                .to_string(),
        );
    }
    hints.truncate(3);
    hints
}

fn usage_hints_for_empty(query: &str, auto_lang: bool, resolved_from: Option<&str>) -> Vec<String> {
    let mut hints = usage_hints(query, auto_lang, 0, false);
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
    pub has_docs: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_files: Vec<String>,
}

pub async fn execute(args: UsageArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let limit = args.limit.unwrap_or(10);
    let max_symbols = args.max_symbols.unwrap_or(50);
    let resolved = resolve_usage_query(app, &args.pattern, args.lang.as_deref()).await?;

    let languages = resolve_usage_languages(app, resolved.language_override.as_deref());
    if languages.is_empty() {
        ctx.print_error(
            OutputError::invalid("Unknown language")
                .with_hint("Run 'symora doctor' to see supported languages."),
        );
        return Ok(());
    }

    let mut symbols = collect_usage_symbols(app, &resolved.query, &languages).await;
    rank_usage_symbols(&mut symbols, &resolved.query);

    if symbols.is_empty() {
        let resolved_from = resolved.resolved_from.clone();
        ctx.print_success(UsageOutput {
            query: resolved.query.clone(),
            resolved_from: resolved_from.clone(),
            filters_applied: vec![],
            analyzed: None,
            section: Section::new(vec![]).with_hints(usage_hints_for_empty(
                &resolved.query,
                resolved.language_override.is_none(),
                resolved_from.as_deref(),
            )),
        });
        return Ok(());
    }

    let filters = args.filter.as_deref().unwrap_or_default();
    let filter_names: Vec<String> = filters.iter().map(|f| f.to_string()).collect();
    let test_matcher = app.test_matcher();

    // Apply pre-filter for NotTestFile (no LSP calls needed)
    if filters.contains(&UsageFilter::NotTestFile) {
        symbols.retain(|s| !test_matcher.is_test_file(&s.location.file));
    }

    // Fast path: sort by name without LSP calls for references
    // Only fetch references for limited results
    let needs_refs_for_sort = matches!(args.sort, SortMetric::References);
    let needs_refs_for_filter = filters.contains(&UsageFilter::HasTests)
        || filters.contains(&UsageFilter::NoTests)
        || filters.contains(&UsageFilter::HasDocs)
        || filters.contains(&UsageFilter::NoDocs)
        || filters.contains(&UsageFilter::ZeroRefs)
        || args.min_refs.is_some();
    let needs_refs = needs_refs_for_sort || needs_refs_for_filter || args.metrics;

    let (items, count, analyzed) = if !needs_refs {
        // Fast path: no LSP reference calls needed
        let mut sorted_symbols = symbols;
        sorted_symbols.sort_by(|a, b| a.name.cmp(&b.name));

        let limited: Vec<UsageResult> = sorted_symbols
            .iter()
            .take(limit)
            .map(|symbol| build_result_without_refs(symbol, ctx.root(), args.snippet))
            .collect();

        let total = sorted_symbols.len();
        (limited, total, None)
    } else if !needs_refs_for_sort && !needs_refs_for_filter {
        // Medium path: sort by name first, then fetch refs only for limited results
        let mut sorted_symbols = symbols;
        sorted_symbols.sort_by(|a, b| a.name.cmp(&b.name));

        let limited_symbols: Vec<_> = sorted_symbols.iter().take(limit).collect();
        let total = sorted_symbols.len();

        let results = fetch_refs_parallel(
            app,
            &limited_symbols,
            ctx.root(),
            &args,
            filters,
            test_matcher,
        )
        .await;
        (results, total, None)
    } else {
        // Slow path: need references for sorting or filtering
        // Limit symbols to analyze for performance (each requires LSP call)
        let symbols_to_process: Vec<_> = symbols.iter().take(max_symbols).collect();
        let analyzed = if symbols.len() > max_symbols {
            Some(max_symbols)
        } else {
            None
        };

        let all_results = fetch_refs_parallel(
            app,
            &symbols_to_process,
            ctx.root(),
            &args,
            filters,
            test_matcher,
        )
        .await;

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

        (limited, total, analyzed)
    };

    let showing = items.len();
    let hints = usage_hints(
        &resolved.query,
        resolved.language_override.is_none(),
        showing,
        analyzed.is_some(),
    );

    let response = UsageOutput {
        query: resolved.query,
        resolved_from: resolved.resolved_from,
        filters_applied: filter_names,
        analyzed,
        section: Section::with_total(items, count).with_hints(hints),
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

    let loc = ParsedLocation::parse(input)?.to_absolute_with_root(Some(app.root()))?;
    let symbols = app
        .lsp
        .find_symbols(&loc.file, FindSymbolsOptions::default().with_depth(10))
        .await?;
    let symbol = find_symbol_at_position(&symbols, loc.line, Some(loc.column))
        .or_else(|| find_symbol_at_position(&symbols, loc.line, None));
    let Some(symbol) = symbol else {
        return Ok(ResolvedUsageQuery {
            query: input.to_string(),
            language_override: lang.map(str::to_string),
            resolved_from: None,
        });
    };

    let inferred_lang = Language::from_path(&loc.file);
    Ok(ResolvedUsageQuery {
        query: symbol.name.clone(),
        language_override: lang.map(str::to_string).or_else(|| {
            (inferred_lang != Language::Unknown).then(|| inferred_lang.lsp_id().to_string())
        }),
        resolved_from: Some(format!(
            "{}:{}:{}",
            ctx_rel(app, &loc.file),
            loc.line,
            loc.column
        )),
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
    test_matcher: &TestMatcher,
) -> Vec<UsageResult> {
    // Use semaphore for fine-grained concurrency control
    // This is faster than batch processing because it keeps MAX_CONCURRENT requests in flight
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_LSP_REQUESTS));

    // Launch all futures immediately (semaphore controls actual concurrency)
    let futures: Vec<_> = symbols
        .iter()
        .map(|symbol| {
            let sem = Arc::clone(&semaphore);
            async move {
                let _permit = sem.acquire().await.ok()?;
                fetch_single_symbol_refs(app, symbol, root, args, filters, test_matcher).await
            }
        })
        .collect();

    let results = join_all(futures).await;

    // Collect non-None results with pre-allocated capacity
    let mut all_results = Vec::with_capacity(symbols.len());
    all_results.extend(results.into_iter().flatten());
    all_results
}

async fn fetch_single_symbol_refs(
    app: &App,
    symbol: &Symbol,
    root: &std::path::Path,
    args: &UsageArgs,
    filters: &[UsageFilter],
    test_matcher: &TestMatcher,
) -> Option<UsageResult> {
    let refs = app
        .lsp
        .find_references(
            &symbol.location.file,
            symbol.location.line,
            symbol.location.column,
        )
        .await
        .unwrap_or_default();

    let ref_count = refs.len();

    // Use iterator to check for tests without collecting all test refs
    let has_tests = refs.iter().any(|r| test_matcher.is_test_file(&r.file));

    if filters.contains(&UsageFilter::HasTests) && !has_tests {
        return None;
    }

    // Filter: only symbols without tests (for test coverage analysis)
    if filters.contains(&UsageFilter::NoTests) && has_tests {
        return None;
    }

    // Filter: only symbols with zero references (dead code detection)
    if filters.contains(&UsageFilter::ZeroRefs) && ref_count > 0 {
        return None;
    }

    // Filter: only symbols with at least N references (find important symbols)
    if let Some(min) = args.min_refs
        && ref_count < min
    {
        return None;
    }

    let needs_docs_check = args.metrics
        || filters.contains(&UsageFilter::HasDocs)
        || filters.contains(&UsageFilter::NoDocs);

    let has_docs = if needs_docs_check {
        app.lsp
            .hover(
                &symbol.location.file,
                symbol.location.line,
                symbol.location.column,
            )
            .await
            .ok()
            .flatten()
            .is_some_and(|h| !h.content.is_empty())
    } else {
        false
    };

    // Filter: only documented symbols
    if filters.contains(&UsageFilter::HasDocs) && !has_docs {
        return None;
    }

    // Filter: only undocumented symbols (for doc coverage analysis)
    if filters.contains(&UsageFilter::NoDocs) && has_docs {
        return None;
    }

    let metrics = if args.metrics {
        // Only collect up to 3 test files (avoid allocating entire list)
        let test_files: Vec<String> = refs
            .iter()
            .filter(|r| test_matcher.is_test_file(&r.file))
            .take(3)
            .map(|r| OutputContext::format_path(&r.file, root))
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

    Some(UsageResult {
        name: symbol.name.clone(),
        file: OutputContext::format_path(&symbol.location.file, root),
        line: symbol.location.line,
        kind: symbol.kind.to_string(),
        signature,
        metrics,
        snippet,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_output_flattens_the_section_contract() {
        let output = UsageOutput {
            query: "AuthUser".to_string(),
            resolved_from: Some("src/main.rs:10:5".to_string()),
            filters_applied: vec![],
            analyzed: Some(10),
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
        assert_eq!(value["count"], 5);
        assert_eq!(value["showing"], 1);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["query"], "AuthUser");
    }

    #[test]
    fn usage_empty_hints_include_file_level_followup_for_resolved_locations() {
        let hints = usage_hints_for_empty("root", true, Some("src/main.py:54:11"));
        assert!(hints.iter().any(|h| h.contains("file-level follow-up")));
    }
}
