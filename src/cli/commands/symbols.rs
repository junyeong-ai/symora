//! Symbols command - find symbols in a file or workspace

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::response::{Section, SymbolOutput};
use crate::cli::utils::extract_signature;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol, SymbolKind};

#[derive(Args, Debug)]
pub struct SymbolsArgs {
    /// File path (or use --name for workspace search)
    #[arg(required_unless_present = "name")]
    pub file: Option<String>,

    /// Search symbols by name across workspace
    #[arg(short, long)]
    pub name: Option<String>,

    /// Filter by symbol path (e.g., "Class/method", "*/update")
    #[arg(short, long)]
    pub symbol: Option<String>,

    /// Language for workspace search
    #[arg(short, long, required_if_eq("name", "name"))]
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
    #[arg(long)]
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

    if let Some(query) = args.name {
        return execute_workspace(
            &query,
            args.lang.as_deref(),
            include_kinds,
            exclude_kinds,
            args.substring,
            args.structural,
            limit,
            app,
        )
        .await;
    }

    let file = match args.file {
        Some(f) => f,
        None => {
            ctx.print_error("File path required when --name not provided");
            return Ok(());
        }
    };

    let path = std::path::Path::new(&file);
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        app.root().join(path)
    };

    let effective_depth = if args.symbol.is_some() && args.depth == 0 {
        10
    } else {
        args.depth
    };

    let need_body = args.body || args.signature;
    let options = FindSymbolsOptions::new().with_depth(effective_depth);
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

            ctx.print_success_flat(Section::with_limit(items, total));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

async fn execute_workspace(
    query: &str,
    lang: Option<&str>,
    include_kinds: Option<Vec<SymbolKind>>,
    exclude_kinds: Option<Vec<SymbolKind>>,
    substring: bool,
    structural: bool,
    limit: usize,
    app: &App,
) -> Result<()> {
    let ctx = &app.output;

    let language = lang
        .map(Language::from_str_loose)
        .unwrap_or(Language::Unknown);

    if language == Language::Unknown {
        ctx.print_error("Language required for workspace search. Use --lang <language>");
        return Ok(());
    }

    match app.lsp.workspace_symbols(query, language).await {
        Ok(symbols) => {
            let filtered = Symbol::filter_advanced(
                &symbols,
                if substring { Some(query) } else { None },
                substring,
                include_kinds.as_deref(),
                exclude_kinds.as_deref(),
                structural,
            );

            let total = filtered.len();
            let limited: Vec<_> = filtered.into_iter().take(limit).collect();

            let items: Vec<SymbolOutput> = limited
                .iter()
                .map(|s| SymbolOutput::from_symbol(s, ctx.root()))
                .collect();

            ctx.print_success_flat(Section::with_limit(items, total));
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
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
