use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::symbol_discovery::{
    broad_symbol_kind_bonus, generic_exact_identifier_penalty, is_probably_test_path,
    noisy_suffix_penalty, symbol_lookup_hints, symbol_match_priority,
};
#[cfg(unix)]
use crate::daemon::DaemonClient;
use crate::infra::ast::{format_query_error, get_node_types, supported_languages};
use crate::infra::file_filter::FileFilter;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Language;

#[derive(Args, Debug)]
#[command(
    after_long_help = "Use `search` when you have a rough name or phrase but not an exact file.\nUse `search symbols` for approximate workspace discovery.\nUse `symbols` once you already know the exact file or exact symbol path.\nTypical flow:\n  1. `symora search symbols auth`\n  2. `symora map file <match>`\n  3. `symora symbols <file>` or `symora symbols --symbol <path>`\n  4. `symora refs <loc>`\n"
)]
pub struct SearchArgs {
    #[command(subcommand)]
    pub command: SearchCommand,
}

#[derive(Subcommand, Debug)]
pub enum SearchCommand {
    /// Fast rough symbol discovery by name or path-like pattern
    Symbols {
        /// Search query
        query: String,

        /// Language for semantic workspace search
        #[arg(short, long = "lang")]
        language: Option<String>,

        /// Symbol kind filter (function, class, struct, etc.)
        #[arg(short, long)]
        kind: Option<String>,

        /// Force semantic workspace-symbol search
        #[arg(long)]
        semantic: bool,

        /// Maximum results
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Fast content lookup by keyword or phrase
    Content {
        /// Search query
        query: String,

        /// Language filter
        #[arg(short, long = "lang")]
        language: Option<String>,

        /// Maximum results
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Structural search using tree-sitter AST patterns
    Ast {
        /// Tree-sitter query pattern, e.g., "(function_definition)"
        pattern: String,

        /// Language (required): python, rust, typescript, etc.
        #[arg(short, long = "lang")]
        language: String,

        /// Search path (defaults to project root)
        #[arg(short, long)]
        path: Option<Vec<PathBuf>>,

        /// Maximum results (0 = unlimited, default from config)
        #[arg(long)]
        limit: Option<usize>,
    },

    /// List available node types for AST search
    Nodes {
        /// Language to list node types for
        #[arg(short, long = "lang")]
        language: String,
    },

    /// Manage search index
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum IndexCommand {
    /// Build or rebuild the search index
    Build {
        /// Force rebuild (ignore existing index)
        #[arg(short, long)]
        force: bool,

        /// Languages to index (comma-separated)
        #[arg(short, long = "lang")]
        languages: Option<String>,
    },

    /// Show index status
    Status,

    /// Clear the search index
    Clear,
}

#[derive(Serialize, Deserialize)]
struct SymbolSearchOutput {
    count: usize,
    #[serde(default)]
    showing: usize,
    #[serde(alias = "results")]
    items: Vec<SymbolResultOutput>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    next_commands: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SymbolResultOutput {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name_path: Option<String>,
    kind: String,
    file: String,
    line: u32,
    column: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    score: f64,
}

#[derive(Serialize, Deserialize)]
struct ContentSearchOutput {
    count: usize,
    #[serde(default)]
    showing: usize,
    #[serde(alias = "results")]
    items: Vec<ContentResultOutput>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    next_commands: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ContentResultOutput {
    file: String,
    line: u32,
    content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    score: f64,
}

#[derive(Serialize, Deserialize)]
struct IndexStatusOutput {
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
    last_indexed: u64,
    #[serde(default)]
    is_indexing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<f32>,
}

#[derive(Serialize, Deserialize)]
struct IndexBuildOutput {
    status: String,
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
}

#[derive(Serialize)]
struct AstSearchOutput {
    count: usize,
    matches: Vec<AstMatchOutput>,
}

#[derive(Serialize)]
struct AstMatchOutput {
    file: String,
    start_line: u32,
    end_line: u32,
    start_column: u32,
    end_column: u32,
    text: String,
    captures: Vec<(String, String)>,
}

#[derive(Serialize)]
struct NodesOutput {
    language: String,
    count: usize,
    node_types: Vec<NodeTypeOutput>,
}

#[derive(Serialize)]
struct NodeTypeOutput {
    category: &'static str,
    node_type: &'static str,
    example: &'static str,
    query: String,
}

// Daemon index build response (different shape from IndexBuildOutput)
#[cfg(unix)]
#[derive(Deserialize)]
struct DaemonIndexBuildOutput {
    stats: DaemonIndexStats,
}

#[cfg(unix)]
#[derive(Deserialize)]
struct DaemonIndexStats {
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
}

pub async fn execute(args: SearchArgs, app: &App) -> Result<()> {
    let cfg = app.config();

    match args.command {
        SearchCommand::Symbols {
            query,
            language,
            kind,
            semantic,
            limit,
        } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_symbol_search(
                app,
                &query,
                language.as_deref(),
                kind.as_deref(),
                semantic,
                limit,
            )
            .await
        }
        SearchCommand::Content {
            query,
            language,
            limit,
        } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_content_search(app, &query, language.as_deref(), limit).await
        }
        SearchCommand::Ast {
            pattern,
            language,
            path,
            limit,
        } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_ast_search(app, &pattern, &language, path, limit).await
        }
        SearchCommand::Nodes { language } => execute_list_nodes(app, &language),
        SearchCommand::Index { command } => execute_index_command(app, command).await,
    }
}

async fn execute_symbol_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    kind: Option<&str>,
    semantic: bool,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;

    let query = query.trim();
    if query.is_empty() {
        ctx.print_error("Search query cannot be empty");
        return Ok(());
    }

    let search_languages = resolve_search_languages(app, language);
    let use_semantic = semantic || looks_like_symbol_path(query);

    if use_semantic {
        return execute_semantic_symbol_search(app, query, kind, limit, &search_languages).await;
    }

    #[cfg(unix)]
    {
        let client = DaemonClient::new(app.root());
        match client.search_symbols(query, Some(limit), kind).await {
            Ok(response) => {
                let mut parsed: SymbolSearchOutput = serde_json::from_value(response)
                    .map_err(|e| anyhow::anyhow!("Invalid daemon response: {}", e))?;

                for r in &mut parsed.items {
                    r.file = ctx.relative_path(&PathBuf::from(&r.file));
                    r.backend = Some("index".to_string());
                }

                if !search_languages.is_empty() && parsed.items.len() < limit {
                    let semantic_results =
                        collect_semantic_symbol_results(app, query, kind, limit, &search_languages)
                            .await;
                    parsed.items =
                        merge_symbol_results(parsed.items, semantic_results, limit, query);
                }

                parsed.showing = parsed.items.len();
                if parsed.count < parsed.showing {
                    parsed.count = parsed.showing;
                }

                parsed.truncated = parsed.items.len() > 1 && parsed.items.len() >= limit;
                parsed.hints = symbol_search_hints(
                    query,
                    language,
                    kind,
                    parsed.truncated,
                    parsed.items.len(),
                );
                parsed.next_commands = symbol_search_next_commands(&parsed.items, query, language);

                ctx.print_success(parsed);
            }
            Err(e) => {
                if should_fallback_to_semantic(&e.to_string(), &search_languages) {
                    return execute_semantic_symbol_search(
                        app,
                        query,
                        kind,
                        limit,
                        &search_languages,
                    )
                    .await;
                }
                ctx.print_error(&format!("Symbol search failed: {}", e));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (kind, limit, language);
        ctx.print_error("Search requires daemon mode (Unix only)");
    }

    Ok(())
}

async fn execute_semantic_symbol_search(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
) -> Result<()> {
    let ctx = &app.output;
    if languages.is_empty() {
        ctx.print_error("No project languages detected for semantic symbol search.");
        return Ok(());
    }

    let mut results = collect_semantic_symbol_results(app, query, kind, limit, languages).await;

    if looks_like_symbol_path(query) && results.len() < limit {
        let expanded =
            collect_document_path_results(app, query, kind, limit, languages, &results).await;
        results = merge_symbol_results(results, expanded, limit, query);
    }

    #[cfg(unix)]
    if looks_like_symbol_path(query) {
        let client = DaemonClient::new(app.root());
        if let Ok(response) = client.search_symbols(query, Some(limit), kind).await
            && let Ok(mut parsed) = serde_json::from_value::<SymbolSearchOutput>(response)
        {
            for result in &mut parsed.items {
                result.file = ctx.relative_path(&PathBuf::from(&result.file));
                result.backend = Some("index".to_string());
            }
            results = merge_symbol_results(results, parsed.items, limit, query);
        }
    }

    let response = SymbolSearchOutput {
        count: results.len(),
        showing: results.len(),
        items: results.clone(),
        truncated: results.len() > 1 && results.len() >= limit,
        hints: symbol_search_hints(
            query,
            None,
            kind,
            results.len() > 1 && results.len() >= limit,
            results.len(),
        ),
        next_commands: symbol_search_next_commands(&results, query, None),
    };
    ctx.print_success(response);
    Ok(())
}

async fn collect_semantic_symbol_results(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
) -> Vec<SymbolResultOutput> {
    let ctx = &app.output;
    let parsed_kind = kind.map(crate::models::symbol::SymbolKind::parse_or_default);
    if languages.is_empty() {
        return Vec::new();
    }

    let semantic_query = workspace_query_from_pattern(query);
    let overfetch_limit = if looks_like_symbol_path(query) {
        limit
    } else {
        limit.saturating_mul(4)
    };
    let mut seen = HashSet::new();
    let mut results = Vec::new();

    for language in languages {
        let Ok(mut symbols) = app.lsp.workspace_symbols(&semantic_query, *language).await else {
            continue;
        };

        for symbol in &mut symbols {
            if let Some(path) = synthesized_symbol_path(symbol) {
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
        score_semantic_symbol(query, b)
            .partial_cmp(&score_semantic_symbol(query, a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut outputs: Vec<_> = results
        .into_iter()
        .take(overfetch_limit)
        .map(|symbol| {
            let score = score_semantic_symbol(query, &symbol);
            SymbolResultOutput {
                name: symbol.name,
                name_path: symbol.name_path,
                kind: symbol.kind.to_string(),
                file: ctx.relative_path(&symbol.location.file),
                line: symbol.location.line,
                column: symbol.location.column,
                container: symbol.container,
                backend: Some("semantic".to_string()),
                score,
            }
        })
        .collect();
    sort_symbol_results(&mut outputs, query);
    prune_low_value_symbol_results(&mut outputs, query, limit);
    outputs
}

fn merge_symbol_results(
    primary: Vec<SymbolResultOutput>,
    secondary: Vec<SymbolResultOutput>,
    limit: usize,
    query: &str,
) -> Vec<SymbolResultOutput> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();

    for result in primary.into_iter().chain(secondary.into_iter()) {
        let symbol_key = result.name_path.as_deref().unwrap_or(&result.name);
        let key = format!("{}:{}:{}", result.file, result.line, symbol_key,);
        if seen.insert(key) {
            merged.push(result);
        }
    }

    sort_symbol_results(&mut merged, query);
    prune_low_value_symbol_results(&mut merged, query, limit);
    merged.truncate(limit);
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
        let semantic_seeds =
            collect_semantic_symbol_results(app, &leaf, kind, limit * 2, languages).await;
        for result in semantic_seeds {
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
                    score: score_semantic_symbol(query, &symbol),
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

    let test_penalty = if is_probably_test_path(&result.file) {
        8
    } else {
        0
    };
    let container_penalty = result
        .container
        .as_deref()
        .map(|container| {
            if container.to_ascii_lowercase().contains("test") {
                3
            } else {
                0
            }
        })
        .unwrap_or(0);
    let kind_penalty = if is_low_signal_kind(&result.kind) {
        6
    } else {
        0
    };
    let suffix_penalty = noisy_suffix_penalty(&name, &q);
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
        - container_penalty
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

fn resolve_search_languages(app: &App, language: Option<&str>) -> Vec<Language> {
    match language.map(Language::parse_or_default) {
        Some(Language::Unknown) => vec![],
        Some(lang) => vec![lang],
        None => detect_languages_by_file_count(app),
    }
}

fn detect_languages_by_file_count(app: &App) -> Vec<Language> {
    let extensions: Vec<&str> = Language::all()
        .into_iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    let filter = FileFilter::with_gitignore(app.root());
    let files = filter.discover_files(&extensions);
    let mut counts: HashMap<Language, usize> = HashMap::new();

    for file in files {
        let language = Language::from_path(&file);
        if language != Language::Unknown {
            *counts.entry(language).or_insert(0) += 1;
        }
    }

    let mut ranked: Vec<_> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.lsp_id().cmp(b.0.lsp_id())));
    ranked.into_iter().map(|(language, _)| language).collect()
}

fn looks_like_symbol_path(query: &str) -> bool {
    query.contains('/') || query.contains('*') || query.contains('[')
}

fn should_fallback_to_semantic(error: &str, languages: &[Language]) -> bool {
    !languages.is_empty() && error.contains("Store not initialized")
}

fn workspace_query_from_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim().trim_start_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let base = last.split('[').next().unwrap_or(last);
    base.trim_matches('*').to_string()
}

fn synthesized_symbol_path(symbol: &crate::models::symbol::Symbol) -> Option<String> {
    let name = symbol.name.trim();
    if name.is_empty() {
        return None;
    }

    let container = symbol
        .container
        .as_deref()
        .unwrap_or_default()
        .replace("::", "/")
        .replace('.', "/")
        .replace('#', "/")
        .replace('\\', "/");
    let container = container.trim_matches('/');
    if container.is_empty() {
        Some(name.to_string())
    } else {
        Some(format!("{}/{}", container, name))
    }
}

fn kind_matches(
    symbol: &crate::models::symbol::Symbol,
    kind: Option<&crate::models::symbol::SymbolKind>,
) -> bool {
    kind.is_none_or(|expected| &symbol.kind == expected)
}

fn score_semantic_symbol(query: &str, symbol: &crate::models::symbol::Symbol) -> f64 {
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

async fn execute_content_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;

    let query = query.trim();
    if query.is_empty() {
        ctx.print_error("Search query cannot be empty");
        return Ok(());
    }

    #[cfg(unix)]
    {
        let client = DaemonClient::new(app.root());
        match client.search_content(query, Some(limit), language).await {
            Ok(response) => {
                let mut parsed: ContentSearchOutput = serde_json::from_value(response)
                    .map_err(|e| anyhow::anyhow!("Invalid daemon response: {}", e))?;

                for r in &mut parsed.items {
                    r.file = ctx.relative_path(&PathBuf::from(&r.file));
                    r.backend = Some("index".to_string());
                }

                parsed.items = prioritize_code_content_results(parsed.items, language, limit);
                parsed.showing = parsed.items.len();
                if parsed.count < parsed.showing {
                    parsed.count = parsed.showing;
                }

                parsed.truncated = parsed.items.len() > 1 && parsed.items.len() >= limit;
                parsed.hints = content_search_hints(query, language, parsed.truncated);
                parsed.next_commands = content_search_next_commands(&parsed.items, language);

                ctx.print_success(parsed);
            }
            Err(e) => {
                if should_fallback_content_search(&e.to_string()) {
                    let parsed = fallback_content_search(app, query, language, limit).await?;
                    ctx.print_success(parsed);
                } else {
                    ctx.print_error(&format!("Content search failed: {}", e));
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        let parsed = fallback_content_search(app, query, language, limit).await?;
        ctx.print_success(parsed);
    }

    Ok(())
}

fn should_fallback_content_search(error: &str) -> bool {
    error.contains("Store not initialized")
}

async fn fallback_content_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<ContentSearchOutput> {
    let language_filter = language.map(Language::parse_or_default);
    let extensions: Vec<&str> = match language_filter {
        Some(Language::Unknown) => Vec::new(),
        Some(lang) => lang.extensions().to_vec(),
        None => Language::all()
            .into_iter()
            .filter(|lang| is_code_language(*lang))
            .flat_map(|lang| lang.extensions().iter().copied())
            .collect(),
    };

    let filter = FileFilter::with_gitignore(app.root());
    let mut files = filter.discover_files(&extensions);
    files.sort();

    let q = query.to_ascii_lowercase();
    let mut results = Vec::new();

    for file in files {
        if results.len() >= limit * 8 {
            break;
        }

        let Ok(metadata) = tokio::fs::metadata(&file).await else {
            continue;
        };
        if metadata.len() > 1_000_000 {
            continue;
        }

        let Ok(content) = tokio::fs::read_to_string(&file).await else {
            continue;
        };

        for (idx, line) in content.lines().enumerate() {
            let score = score_content_line(&q, line);
            if score <= 0.0 {
                continue;
            }

            results.push(ContentResultOutput {
                file: app.output.relative_path(&file),
                line: idx as u32 + 1,
                content: line.to_string(),
                backend: Some("scan".to_string()),
                score,
            });
        }
    }

    let total = results.len();
    results = prioritize_code_content_results(results, language, limit);

    Ok(ContentSearchOutput {
        count: total,
        showing: results.len(),
        items: results.clone(),
        truncated: results.len() > 1 && results.len() >= limit,
        hints: content_search_hints(query, language, results.len() > 1 && results.len() >= limit),
        next_commands: content_search_next_commands(&results, language),
    })
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

fn content_search_hints(query: &str, language: Option<&str>, truncated: bool) -> Vec<String> {
    let mut hints = Vec::new();
    if truncated {
        hints.push("Narrow results with a longer query phrase or increase --limit".to_string());
    }
    if language.is_none() {
        hints.push("Add --lang to limit content search to one language".to_string());
    }
    if !query.contains(' ') {
        hints.push(
            "Use a more specific multi-token phrase when broad keyword matches are noisy"
                .to_string(),
        );
    }
    hints.truncate(3);
    hints
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

fn content_search_next_commands(
    results: &[ContentResultOutput],
    language: Option<&str>,
) -> Vec<String> {
    if results.len() <= 1 {
        return Vec::new();
    }

    let mut commands = Vec::new();
    if let Some(first) = results.first() {
        commands.push(format!("symora map file {} --related-limit 5", first.file));
        commands.push(format!("symora symbols {} --depth 1", first.file));
        if language.is_none() {
            let lang = Language::from_path(std::path::Path::new(&first.file));
            if lang != Language::Unknown {
                commands.push(format!(
                    "symora search content '{}' --lang {}",
                    first.content.trim(),
                    lang.lsp_id()
                ));
            }
        }
    }
    commands.truncate(3);
    commands
}

fn prioritize_code_content_results(
    mut results: Vec<ContentResultOutput>,
    language: Option<&str>,
    limit: usize,
) -> Vec<ContentResultOutput> {
    if language.is_none() {
        let code_count = results
            .iter()
            .filter(|result| {
                is_code_language(Language::from_path(std::path::Path::new(&result.file)))
            })
            .count();
        if code_count >= limit {
            results.retain(|result| {
                is_code_language(Language::from_path(std::path::Path::new(&result.file)))
            });
        }
    }

    results.sort_by(|a, b| {
        content_result_priority(b, language)
            .cmp(&content_result_priority(a, language))
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    results.truncate(limit);
    results
}

fn content_result_priority(result: &ContentResultOutput, language: Option<&str>) -> i32 {
    let language_kind = Language::from_path(std::path::Path::new(&result.file));
    let mut priority = 0;
    if language.is_none() && is_code_language(language_kind) {
        priority += 10;
    }
    if result.backend.as_deref() == Some("index") {
        priority += 1;
    }
    priority
}

fn is_code_language(language: Language) -> bool {
    !matches!(
        language,
        Language::Markdown | Language::Toml | Language::Yaml
    )
}

fn score_content_line(query: &str, line: &str) -> f64 {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return 0.0;
    }

    let lower = line.to_ascii_lowercase();
    if !lower.contains(query) {
        return 0.0;
    }

    if trimmed.to_ascii_lowercase().starts_with(query) {
        1.0
    } else if line.len() < 80 {
        0.85
    } else if lower.find(query).is_some_and(|idx| idx <= 20) {
        0.7
    } else if line.len() < 150 {
        0.5
    } else {
        0.3
    }
}

async fn execute_index_command(app: &App, command: IndexCommand) -> Result<()> {
    let ctx = &app.output;

    #[cfg(unix)]
    {
        let client = DaemonClient::new(app.root());

        match command {
            IndexCommand::Build { force, languages } => {
                let langs: Option<Vec<String>> = languages.map(|s| {
                    s.split(',')
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                });

                match client.index_build(force, langs).await {
                    Ok(response) => {
                        let parsed: DaemonIndexBuildOutput = serde_json::from_value(response)
                            .map_err(|e| anyhow::anyhow!("Invalid daemon response: {}", e))?;
                        ctx.print_success(IndexBuildOutput {
                            status: "completed".to_string(),
                            file_count: parsed.stats.file_count,
                            symbol_count: parsed.stats.symbol_count,
                            content_line_count: parsed.stats.content_line_count,
                            index_size_bytes: parsed.stats.index_size_bytes,
                        });
                    }
                    Err(e) => {
                        ctx.print_error(&format!("Index build failed: {}", e));
                    }
                }
            }
            IndexCommand::Status => match client.index_status().await {
                Ok(response) => {
                    let parsed: IndexStatusOutput = serde_json::from_value(response)
                        .map_err(|e| anyhow::anyhow!("Invalid daemon response: {}", e))?;
                    ctx.print_success(parsed);
                }
                Err(e) => {
                    ctx.print_error(&format!("Failed to get index status: {}", e));
                }
            },
            IndexCommand::Clear => match client.index_clear().await {
                Ok(_) => {
                    ctx.print_success(serde_json::json!({ "cleared": true }));
                }
                Err(e) => {
                    ctx.print_error(&format!("Failed to clear index: {}", e));
                }
            },
        }
    }

    #[cfg(not(unix))]
    {
        let _ = command;
        ctx.print_error("Search index requires daemon mode (Unix only)");
    }

    Ok(())
}

/// Normalize AST pattern by auto-wrapping simple node types with parentheses.
fn normalize_ast_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim();

    if trimmed.starts_with('(') {
        return trimmed.to_string();
    }

    let is_simple = !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');

    if is_simple {
        format!("({})", trimmed)
    } else {
        trimmed.to_string()
    }
}

async fn execute_ast_search(
    app: &App,
    pattern: &str,
    language: &str,
    path: Option<Vec<PathBuf>>,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;

    let pattern = pattern.trim();
    if pattern.is_empty() {
        ctx.print_error(
            "AST pattern cannot be empty.\n\
             Example: function_definition or (function_definition)\n\
             Use 'symora search nodes -l <lang>' to see available node types.",
        );
        return Ok(());
    }

    let normalized_pattern = normalize_ast_pattern(pattern);
    let pattern = &normalized_pattern;

    let lang = parse_language(language)?;
    let paths = path.unwrap_or_else(|| vec![app.root().to_path_buf()]);

    match app.ast.query(pattern, lang, &paths).await {
        Ok(matches) => {
            let limited: Vec<_> = if limit == 0 {
                matches
            } else {
                matches.into_iter().take(limit).collect()
            };
            let response = AstSearchOutput {
                count: limited.len(),
                matches: limited
                    .iter()
                    .map(|m| AstMatchOutput {
                        file: ctx.relative_path(&m.file),
                        start_line: m.start_line,
                        end_line: m.end_line,
                        start_column: m.start_column,
                        end_column: m.end_column,
                        text: m.text.clone(),
                        captures: m.captures.clone(),
                    })
                    .collect(),
            };
            ctx.print_success(response);
        }
        Err(crate::error::SearchError::InvalidPattern(e)) => {
            ctx.print_error(&format_query_error(lang, &e));
        }
        Err(crate::error::SearchError::UnsupportedLanguage(l)) => {
            let supported: Vec<_> = supported_languages().iter().map(|l| l.lsp_id()).collect();
            ctx.print_error(&format!(
                "AST search not supported for {:?}.\n\nSupported languages: {}",
                l,
                supported.join(", ")
            ));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

fn execute_list_nodes(app: &App, language: &str) -> Result<()> {
    let ctx = &app.output;
    let lang = parse_language(language)?;

    let nodes = get_node_types(lang);

    if nodes.is_empty() {
        let supported: Vec<_> = supported_languages().iter().map(|l| l.lsp_id()).collect();
        ctx.print_error(&format!(
            "AST search not supported for '{}'.\n\nSupported languages: {}",
            language,
            supported.join(", ")
        ));
        return Ok(());
    }

    let response = NodesOutput {
        language: lang.lsp_id().to_string(),
        count: nodes.len(),
        node_types: nodes
            .iter()
            .map(|n| NodeTypeOutput {
                category: n.category,
                node_type: n.node_type,
                example: n.example,
                query: format!("({})", n.node_type),
            })
            .collect(),
    };

    ctx.print_success(response);
    Ok(())
}

fn parse_language(lang: &str) -> Result<Language> {
    lang.parse::<Language>().map_err(|_| {
        let supported: Vec<_> = supported_languages().iter().map(|l| l.lsp_id()).collect();
        anyhow::anyhow!(
            "Unknown language: '{}'\n\nFor AST search, supported: {}",
            lang,
            supported.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_hints_are_empty_for_exact_single_result() {
        let hints = symbol_search_hints("SearchCommand/Content", None, None, false, 1);
        assert!(hints.is_empty());
    }

    #[test]
    fn symbol_search_output_uses_items_field() {
        let output = SymbolSearchOutput {
            count: 3,
            showing: 1,
            items: vec![SymbolResultOutput {
                name: "SearchCommand".to_string(),
                name_path: Some("SearchCommand".to_string()),
                kind: "enum".to_string(),
                file: "src/cli/commands/search.rs".to_string(),
                line: 30,
                column: 1,
                container: None,
                backend: Some("index".to_string()),
                score: 1.0,
            }],
            truncated: true,
            hints: vec!["narrow it".to_string()],
            next_commands: vec!["symora symbols src/cli/commands/search.rs --depth 1".to_string()],
        };

        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["count"], 3);
        assert_eq!(value["showing"], 1);
        assert!(value.get("items").is_some());
        assert!(value.get("results").is_none());
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn content_search_output_uses_items_field() {
        let output = ContentSearchOutput {
            count: 10,
            showing: 2,
            items: vec![ContentResultOutput {
                file: "src/main.rs".to_string(),
                line: 10,
                content: "async fn run() {}".to_string(),
                backend: Some("scan".to_string()),
                score: 1.0,
            }],
            truncated: true,
            hints: vec![],
            next_commands: vec![],
        };

        let value = serde_json::to_value(output).unwrap();
        assert!(value.get("items").is_some());
        assert!(value.get("results").is_none());
        assert_eq!(value["count"], 10);
        assert_eq!(value["showing"], 2);
    }
}
