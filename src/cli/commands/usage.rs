//! Usage Finder - search for usage examples with metrics

use std::sync::Arc;

use anyhow::Result;
use clap::{Args, ValueEnum};
use futures::future::join_all;
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::app::App;
use crate::cli::utils::is_test_file;
use crate::models::symbol::Language;
use crate::models::symbol::Symbol;

/// Maximum concurrent LSP requests (higher = faster but more LSP server load)
const MAX_CONCURRENT_LSP_REQUESTS: usize = 20;

#[derive(Args, Debug)]
pub struct UsageArgs {
    /// Search pattern (symbol name or regex)
    pub pattern: String,

    /// Sort results by metric
    #[arg(long, short, default_value = "references")]
    pub sort: SortMetric,

    /// Filter results (comma-separated)
    #[arg(long, short, value_delimiter = ',')]
    pub filter: Option<Vec<Filter>>,

    /// Include metrics in output
    #[arg(long)]
    pub with_metrics: bool,

    /// Include code snippet
    #[arg(long)]
    pub with_snippet: bool,

    /// Maximum results to display
    #[arg(long, default_value = "10")]
    pub limit: usize,

    /// Maximum symbols to analyze (for --sort references performance)
    /// Use smaller values for faster results on large codebases
    #[arg(long, default_value = "50")]
    pub max_symbols: usize,

    /// Language filter (for workspace search)
    #[arg(long, short)]
    pub lang: Option<String>,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum SortMetric {
    References,
    Name,
}

#[derive(Debug, Clone, ValueEnum, PartialEq)]
pub enum Filter {
    /// Only show symbols that have tests
    HasTests,
    /// Only show symbols that have documentation
    HasDocs,
    /// Only show symbols that lack documentation (for doc coverage)
    NoDocs,
    /// Exclude symbols defined in test files
    NotTestFile,
}

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub query: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters_applied: Vec<String>,
    pub results: Vec<UsageResult>,
    /// Total number of matching symbols (before limit applied)
    pub count: usize,
    /// Number of results actually returned (after limit)
    pub showing: usize,
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

    let language = args
        .lang
        .as_ref()
        .map(|l| Language::from_str_loose(l))
        .unwrap_or(Language::Unknown);

    if language == Language::Unknown && args.lang.is_some() {
        ctx.print_error("Unknown language. Run 'symora doctor' to see supported languages.");
        return Ok(());
    }

    let mut symbols = if language != Language::Unknown {
        app.lsp.workspace_symbols(&args.pattern, language).await?
    } else {
        ctx.print_error("Language is required for usage search. Use --lang <language>.");
        return Ok(());
    };

    if symbols.is_empty() {
        ctx.print_success_flat(UsageResponse {
            query: args.pattern.clone(),
            filters_applied: vec![],
            results: vec![],
            count: 0,
            showing: 0,
        });
        return Ok(());
    }

    let filters = args.filter.as_deref().unwrap_or_default();
    let filter_names: Vec<String> = filters.iter().map(|f| format!("{:?}", f)).collect();

    // Apply pre-filter for NotTestFile (no LSP calls needed)
    if filters.contains(&Filter::NotTestFile) {
        symbols.retain(|s| !is_test_file(&s.location.file));
    }

    // Fast path: sort by name without LSP calls for references
    // Only fetch references for limited results
    let needs_refs_for_sort = matches!(args.sort, SortMetric::References);
    let needs_refs_for_filter = filters.contains(&Filter::HasTests)
        || filters.contains(&Filter::HasDocs)
        || filters.contains(&Filter::NoDocs);
    let needs_refs = needs_refs_for_sort || needs_refs_for_filter || args.with_metrics;

    let results = if !needs_refs {
        // Fast path: no LSP reference calls needed
        let mut sorted_symbols = symbols;
        sorted_symbols.sort_by(|a, b| a.name.cmp(&b.name));

        let limited: Vec<UsageResult> = sorted_symbols
            .iter()
            .take(args.limit)
            .map(|symbol| build_result_without_refs(symbol, ctx.root(), args.with_snippet))
            .collect();

        let total = sorted_symbols.len();
        (limited, total)
    } else if !needs_refs_for_sort && !needs_refs_for_filter {
        // Medium path: sort by name first, then fetch refs only for limited results
        let mut sorted_symbols = symbols;
        sorted_symbols.sort_by(|a, b| a.name.cmp(&b.name));

        let limited_symbols: Vec<_> = sorted_symbols.iter().take(args.limit).collect();
        let total = sorted_symbols.len();

        let results = fetch_refs_parallel(app, &limited_symbols, ctx.root(), &args, filters).await;
        (results, total)
    } else {
        // Slow path: need references for sorting or filtering
        // Limit symbols to analyze for performance (each requires LSP call)
        let symbols_to_process: Vec<_> = symbols.iter().take(args.max_symbols).collect();
        let truncated = symbols.len() > args.max_symbols;

        let all_results =
            fetch_refs_parallel(app, &symbols_to_process, ctx.root(), &args, filters).await;

        if truncated {
            eprintln!(
                "Note: Analyzed {}/{} symbols. Use --max-symbols to increase.",
                args.max_symbols,
                symbols.len()
            );
        }

        let total = all_results.len();
        let mut with_refs: Vec<_> = all_results
            .into_iter()
            .map(|r| {
                let refs = r.metrics.as_ref().map(|m| m.references).unwrap_or(0);
                (r, refs)
            })
            .collect();

        match args.sort {
            SortMetric::References => with_refs.sort_by(|a, b| b.1.cmp(&a.1)),
            SortMetric::Name => with_refs.sort_by(|a, b| a.0.name.cmp(&b.0.name)),
        }

        // Strip metrics if user didn't request them (they were only used for sorting/filtering)
        let limited: Vec<UsageResult> = with_refs
            .into_iter()
            .take(args.limit)
            .map(|(mut r, _)| {
                if !args.with_metrics {
                    r.metrics = None;
                }
                r
            })
            .collect();

        (limited, total)
    };

    let showing = results.0.len();

    let response = UsageResponse {
        query: args.pattern,
        filters_applied: filter_names,
        results: results.0,
        count: results.1,
        showing,
    };

    ctx.print_success_flat(response);

    Ok(())
}

fn build_result_without_refs(
    symbol: &Symbol,
    root: &std::path::Path,
    with_snippet: bool,
) -> UsageResult {
    let signature = crate::cli::utils::extract_signature(symbol.body.as_deref());
    let snippet = if with_snippet {
        symbol.body.clone()
    } else {
        None
    };

    UsageResult {
        name: symbol.name.clone(),
        file: symbol
            .location
            .file
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| symbol.location.file.display().to_string()),
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
    filters: &[Filter],
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
                fetch_single_symbol_refs(app, symbol, root, args, filters).await
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
    filters: &[Filter],
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
    let has_tests = refs.iter().any(|r| is_test_file(&r.file));

    if filters.contains(&Filter::HasTests) && !has_tests {
        return None;
    }

    let needs_docs_check = args.with_metrics
        || filters.contains(&Filter::HasDocs)
        || filters.contains(&Filter::NoDocs);

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
    if filters.contains(&Filter::HasDocs) && !has_docs {
        return None;
    }

    // Filter: only undocumented symbols (for doc coverage analysis)
    if filters.contains(&Filter::NoDocs) && has_docs {
        return None;
    }

    let metrics = if args.with_metrics {
        // Only collect up to 3 test files (avoid allocating entire list)
        let test_files: Vec<String> = refs
            .iter()
            .filter(|r| is_test_file(&r.file))
            .take(3)
            .map(|r| {
                r.file
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| r.file.display().to_string())
            })
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

    let snippet = if args.with_snippet {
        symbol.body.clone()
    } else {
        None
    };

    let signature = crate::cli::utils::extract_signature(symbol.body.as_deref());

    Some(UsageResult {
        name: symbol.name.clone(),
        file: symbol
            .location
            .file
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| symbol.location.file.display().to_string()),
        line: symbol.location.line,
        kind: symbol.kind.to_string(),
        signature,
        metrics,
        snippet,
    })
}
