use anyhow::Result;
use clap::Args;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::resolve_project_file;
use crate::cli::response::{Section, SymbolOutput};
use crate::cli::symbol_discovery::{
    LOW_SIGNAL_KIND_PENALTY, TEST_FILE_PENALTY, broad_symbol_kind_bonus,
    generic_exact_identifier_penalty, noisy_suffix_penalty, symbol_lookup_hints,
    symbol_match_priority,
};
use crate::cli::utils::extract_signature;
use crate::infra::file_filter::FileFilter;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Location, Symbol, SymbolKind};
use crate::services::TestScope;
use crate::services::store::{SymbolExtractor, SymbolSearchResult};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Use `symbols <file>` to inspect one file semantically.\nUse `symbols --symbol <path>` when you already know a symbol path or pattern (a name, Class/method, or */method).\nUse `symbols --name` only when the symbol name is already fairly specific.\nUse `search symbols` first when the query is still broad or approximate.\n"
)]
pub struct SymbolsArgs {
    /// File path (or use --name for workspace search)
    #[arg(required_unless_present_any = ["name", "symbol"])]
    pub file: Option<String>,

    /// Search fairly specific symbol names across workspace
    #[arg(short, long)]
    pub name: Option<String>,

    /// Filter by symbol path (e.g., "Class/method", "*/update")
    #[arg(short, long)]
    pub symbol: Option<String>,

    /// Language for workspace search
    #[arg(short, long)]
    pub lang: Option<String>,

    /// Include symbol body
    #[arg(short, long, conflicts_with = "signature")]
    pub body: bool,

    /// Include only signature
    #[arg(long, conflicts_with = "body")]
    pub signature: bool,

    /// Include nested symbols up to depth
    #[arg(short, long, default_value = "0")]
    pub depth: u32,

    /// Filter by symbol kind(s), comma-separated
    #[arg(long, short = 'k')]
    pub kind: Option<String>,

    /// Exclude symbol kind(s), comma-separated
    #[arg(long)]
    pub exclude: Option<String>,

    /// Use substring matching
    #[arg(long)]
    pub substring: bool,

    /// Exclude low-level symbols (variables, constants)
    #[arg(long)]
    pub structural: bool,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: SymbolsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();
    let limit = args.limit.unwrap_or(cfg.lsp.symbol_limit);

    let include_kinds = parse_kind_list(&args.kind)?;
    let exclude_kinds = parse_kind_list(&args.exclude)?;

    if args.name.is_some() || (args.file.is_none() && args.symbol.is_some()) {
        return execute_workspace(
            WorkspaceParams {
                name_query: args.name.as_deref(),
                symbol_query: args.symbol.as_deref(),
                lang: args.lang.as_deref(),
                include_kinds,
                exclude_kinds,
                substring: args.substring,
                structural: args.structural,
                body: args.body,
                signature: args.signature,
                limit,
            },
            app,
        )
        .await;
    }

    let file = match args.file {
        Some(f) => f,
        None => {
            ctx.print_error(OutputError::invalid(
                "File path required when --name not provided",
            ));
            return Ok(());
        }
    };

    let abs_path = match resolve_project_file(std::path::Path::new(&file), app.root()) {
        Ok(p) => p,
        Err(e) => {
            ctx.print_error(e);
            return Ok(());
        }
    };

    let effective_depth = if args.symbol.is_some() && args.depth == 0 {
        10
    } else {
        args.depth
    };

    let need_body = args.body || args.signature;
    let options = FindSymbolsOptions::default().with_depth(effective_depth);
    let options = if need_body {
        options.with_body()
    } else {
        options
    };

    match app.lsp.find_symbols(&abs_path, options).await {
        Ok(mut symbols) => {
            Symbol::compute_paths_for_all(&mut symbols);

            let filtered = Symbol::filter_advanced(
                &symbols,
                args.symbol.as_deref(),
                args.substring,
                include_kinds.as_deref(),
                exclude_kinds.as_deref(),
                args.structural,
            );

            let total = filtered.len();
            let limited: Vec<_> = filtered.into_iter().take(limit).collect();

            let items: Vec<SymbolOutput> = limited
                .iter()
                .map(|s| {
                    let mut output = SymbolOutput::from_symbol(s, ctx.root());
                    if args.signature {
                        let sig = extract_signature(s.body.as_deref());
                        output = output.with_signature(sig).without_body();
                    }
                    output
                })
                .collect();

            ctx.print_success(Section::with_total(items, total));
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

struct WorkspaceParams<'a> {
    name_query: Option<&'a str>,
    symbol_query: Option<&'a str>,
    lang: Option<&'a str>,
    include_kinds: Option<Vec<SymbolKind>>,
    exclude_kinds: Option<Vec<SymbolKind>>,
    substring: bool,
    structural: bool,
    body: bool,
    signature: bool,
    limit: usize,
}

async fn execute_workspace(params: WorkspaceParams<'_>, app: &App) -> Result<()> {
    let WorkspaceParams {
        name_query,
        symbol_query,
        lang,
        include_kinds,
        exclude_kinds,
        substring,
        structural,
        body,
        signature,
        limit,
    } = params;
    let ctx = &app.output;

    let languages = resolve_workspace_languages(app, lang);
    if languages.is_empty() {
        let valid = Language::all()
            .iter()
            .map(|l| l.lsp_id())
            .collect::<Vec<_>>()
            .join(", ");
        ctx.print_error(OutputError::invalid(format!(
            "No valid workspace languages available. Valid languages: {}",
            valid
        )));
        return Ok(());
    }

    let query = effective_workspace_query(name_query, symbol_query);
    if query.is_empty() {
        ctx.print_error(OutputError::invalid(
            "Workspace symbol query cannot be empty",
        ));
        return Ok(());
    }

    let mut symbols = Vec::new();
    let mut seen = HashSet::new();

    // Index-primary: the tree-sitter index carries every symbol — including the
    // methods rust-analyzer's workspace/symbol recall routinely drops — under
    // the same canonical name_path the other surfaces use, so a `Type/method`
    // path resolves here even when the live server omits it. The LSP pass below
    // then supplements languages the index does not extract and any edits made
    // since the last build.
    // An explicit `--lang` scopes the index query to that language (the LSP
    // pass below is already per-language); without it the index spans every
    // indexed language, matching the workspace-wide intent of a bare query.
    let index_lang = lang
        .map(Language::parse_or_default)
        .filter(|l| *l != Language::Unknown);
    if let Ok(page) = app
        .store
        .search_symbols(&query, limit.saturating_mul(2), None, index_lang)
        .await
    {
        for row in page.rows {
            let symbol = symbol_from_index_row(row);
            if seen.insert(workspace_dedup_key(&symbol)) {
                symbols.push(symbol);
            }
        }
    }

    let mut indexing = None;
    for language in &languages {
        let Ok(batch) = app.lsp.workspace_symbols(&query, *language).await else {
            continue;
        };
        // Authoritativeness is PER-LANGUAGE: this is an index-primary answer, so
        // an indexed language's LSP pass is enrichment over the authoritative
        // index and its warmup marker is dropped, but an UNINDEXED language's LSP
        // is its sole source — disclose only that, so a bare query spanning both
        // still surfaces the unindexed language's timeout.
        if !SymbolExtractor::is_supported(*language) {
            indexing = indexing.or(batch.indexing);
        }
        let mut batch = batch.data;
        for symbol in &mut batch {
            if let Some(path) = symbol.workspace_name_path() {
                symbol.name_path = Some(path);
            }
        }
        for symbol in batch {
            if seen.insert(workspace_dedup_key(&symbol)) {
                symbols.push(symbol);
            }
        }

        if symbols.len() >= limit.saturating_mul(2) {
            break;
        }
    }

    let pattern = symbol_query.or(name_query.filter(|_| substring));
    let filtered = Symbol::filter_advanced(
        &symbols,
        pattern,
        substring,
        include_kinds.as_deref(),
        exclude_kinds.as_deref(),
        structural,
    );

    let mut filtered = filtered;
    if name_query.is_some() && symbol_query.is_none() {
        sort_workspace_symbols(&mut filtered, &query, app.test_scope());
        prune_low_value_workspace_symbols(&mut filtered, &query, limit, app.test_scope());
    }

    let total = filtered.len();
    let limited: Vec<_> = filtered.into_iter().take(limit).collect();

    let items: Vec<SymbolOutput> = if body || signature {
        workspace_symbol_bodies(app, &limited, signature).await
    } else {
        limited
            .iter()
            .map(|s| SymbolOutput::from_symbol(s, ctx.root()))
            .collect()
    };
    let item_count = items.len();
    let truncated = item_count < total;
    let hints = workspace_symbol_hints(
        name_query,
        symbol_query,
        lang,
        include_kinds.is_none(),
        truncated,
        item_count,
    );

    ctx.print_success(
        Section::with_total(items, total)
            .with_hints(hints)
            .with_indexing(indexing),
    );

    Ok(())
}

/// Build a `Symbol` from an index search row. The index records the symbol's
/// start position; a workspace result is addressed and navigated by that point,
/// exactly as rust-analyzer's own workspace symbols are (their location is the
/// name span too), so this is the same shape the LSP pass produces.
fn symbol_from_index_row(row: SymbolSearchResult) -> Symbol {
    let mut symbol = Symbol::new(
        row.name,
        row.kind,
        Location::point(row.file, row.line, row.column),
    );
    symbol.name_path = row.name_path;
    symbol.container = row.container;
    symbol
}

/// Dedup key for merging the index and LSP workspace passes: the same symbol is
/// reported at the same file, position, and path regardless of its source.
fn workspace_dedup_key(symbol: &Symbol) -> String {
    format!(
        "{}:{}:{}:{}",
        symbol.location.file.display(),
        symbol.location.line,
        symbol.location.column,
        symbol.path()
    )
}

/// Attach bodies to resolved workspace symbols. The index and `workspace/symbol`
/// surfaces carry only a name-span location, so the body is read from each
/// file's documentSymbol tree (which has the full range) — one request per
/// distinct file, matched back by the canonical `name_path` every producer
/// agrees on. A symbol the document tree does not surface keeps its bodiless
/// row rather than a wrong slice.
async fn workspace_symbol_bodies(
    app: &App,
    resolved: &[Symbol],
    signature: bool,
) -> Vec<SymbolOutput> {
    let ctx = &app.output;
    let options = FindSymbolsOptions::default()
        .with_depth(u32::MAX)
        .with_body();

    let mut bodied: HashMap<(PathBuf, String), Symbol> = HashMap::new();
    let mut fetched: HashSet<PathBuf> = HashSet::new();
    for symbol in resolved {
        let file = symbol.location.file.clone();
        if !fetched.insert(file.clone()) {
            continue;
        }
        if let Ok(mut tree) = app.lsp.find_symbols(&file, options.clone()).await {
            Symbol::compute_paths_for_all(&mut tree);
            collect_bodied(&tree, &file, &mut bodied);
        }
    }

    resolved
        .iter()
        .map(|symbol| {
            let key = (symbol.location.file.clone(), symbol.path().to_string());
            let source = bodied.get(&key).unwrap_or(symbol);
            let mut output = SymbolOutput::from_symbol(source, ctx.root());
            if signature {
                let sig = extract_signature(source.body.as_deref());
                output = output.with_signature(sig).without_body();
            }
            output
        })
        .collect()
}

/// Index a file's documentSymbol tree by `(file, name_path)` so a resolved
/// workspace symbol can claim its full body.
fn collect_bodied(symbols: &[Symbol], file: &Path, out: &mut HashMap<(PathBuf, String), Symbol>) {
    for symbol in symbols {
        out.insert(
            (file.to_path_buf(), symbol.path().to_string()),
            symbol.clone(),
        );
        if !symbol.children.is_empty() {
            collect_bodied(&symbol.children, file, out);
        }
    }
}

fn resolve_workspace_languages(app: &App, lang: Option<&str>) -> Vec<Language> {
    match lang.map(Language::parse_or_default) {
        Some(Language::Unknown) => vec![],
        Some(language) => vec![language],
        None => detect_languages_by_file_count(app),
    }
}

fn detect_languages_by_file_count(app: &App) -> Vec<Language> {
    let extensions: Vec<&str> = Language::all()
        .into_iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    let filter = FileFilter::new(app.root());
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

fn workspace_symbol_hints(
    name_query: Option<&str>,
    symbol_query: Option<&str>,
    lang: Option<&str>,
    no_kind_filter: bool,
    truncated: bool,
    result_count: usize,
) -> Vec<String> {
    symbol_lookup_hints(
        name_query.or(symbol_query).unwrap_or_default(),
        symbol_query.is_some(),
        lang.is_none(),
        no_kind_filter && name_query.is_some(),
        truncated,
        result_count,
    )
}

fn sort_workspace_symbols(symbols: &mut [Symbol], query: &str, test_scope: &TestScope) {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    symbols.sort_by(|a, b| {
        workspace_symbol_priority(b, &q, test_scope)
            .cmp(&workspace_symbol_priority(a, &q, test_scope))
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.location.file.cmp(&b.location.file))
            .then_with(|| a.location.line.cmp(&b.location.line))
            .then_with(|| a.location.column.cmp(&b.location.column))
    });
}

fn workspace_symbol_priority(symbol: &Symbol, query: &str, test_scope: &TestScope) -> i32 {
    let name = symbol.name.to_ascii_lowercase();
    let path = symbol.path().to_ascii_lowercase();
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
    let suffix_penalty = noisy_suffix_penalty(&name, query);
    let generic_exact_penalty = generic_exact_identifier_penalty(
        query,
        &name,
        &symbol.kind.to_string(),
        symbol.kind.is_low_level(),
    );
    let kind_bonus = broad_symbol_kind_bonus(
        query,
        &name,
        &symbol.kind.to_string(),
        symbol.kind.is_low_level(),
    );

    match_priority + kind_bonus
        - test_penalty
        - kind_penalty
        - suffix_penalty
        - generic_exact_penalty
}

fn prune_low_value_workspace_symbols(
    symbols: &mut Vec<Symbol>,
    query: &str,
    limit: usize,
    test_scope: &TestScope,
) {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let high_value_count = symbols
        .iter()
        .filter(|symbol| is_high_value_workspace_symbol(symbol, &q, test_scope))
        .count();

    if high_value_count >= usize::min(limit, 3) {
        symbols.retain(|symbol| is_high_value_workspace_symbol(symbol, &q, test_scope));
    }
}

fn is_high_value_workspace_symbol(symbol: &Symbol, query: &str, test_scope: &TestScope) -> bool {
    let name = symbol.name.to_ascii_lowercase();
    !test_scope.is_test_file(&symbol.location.file)
        && !symbol.kind.is_low_level()
        && noisy_suffix_penalty(&name, query) == 0
}

fn effective_workspace_query(name_query: Option<&str>, symbol_query: Option<&str>) -> String {
    if let Some(name) = name_query.map(str::trim).filter(|value| !value.is_empty()) {
        return name.to_string();
    }

    symbol_query
        .map(str::trim)
        .and_then(|pattern| {
            let trimmed = pattern.trim_start_matches('/');
            let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
            let base = last.split('[').next().unwrap_or(last);
            let candidate = base.trim_matches('*').trim();
            (!candidate.is_empty()).then(|| candidate.to_string())
        })
        .unwrap_or_default()
}

fn parse_kind_list(kind_str: &Option<String>) -> Result<Option<Vec<SymbolKind>>> {
    let Some(kinds) = kind_str else {
        return Ok(None);
    };

    let mut result = Vec::new();
    for k in kinds.split(',') {
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        match k.parse::<SymbolKind>() {
            Ok(kind) => result.push(kind),
            Err(_) => anyhow::bail!(
                "Unknown symbol kind: '{}'. Valid: {}",
                k,
                SymbolKind::all_kind_names().join(", ")
            ),
        }
    }

    Ok(if result.is_empty() {
        None
    } else {
        Some(result)
    })
}
