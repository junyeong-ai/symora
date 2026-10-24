use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::resolve_project_file;
use crate::cli::response::{Section, SymbolOutput, SymbolSource};
use crate::cli::utils::extract_signature;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{SymbolFilter, SymbolKind};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Use `symbols <file>` to inspect one file semantically.\nAdd --symbol <path> to narrow it to a name, Class/method, or */method pattern.\nUse `search symbols` to find a declaration whose file you do not know; its --body and --signature read the source the same way.\n"
)]
pub struct SymbolsArgs {
    /// File path
    pub file: String,

    /// Filter by symbol path (e.g., "Class/method", "*/update")
    #[arg(short, long)]
    pub symbol: Option<String>,

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
    let limit = args.limit.unwrap_or(app.config().lsp.symbol_limit);

    let include_kinds = parse_kind_list(&args.kind)?;
    let exclude_kinds = parse_kind_list(&args.exclude)?;

    let abs_path = match resolve_project_file(std::path::Path::new(&args.file), app.root()) {
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

    let source = SymbolSource::new(args.body, args.signature);
    let options = FindSymbolsOptions::default().with_depth(effective_depth);
    let options = match source {
        SymbolSource::Location => options,
        SymbolSource::Signature | SymbolSource::Body => options.with_body(),
    };

    match crate::cli::declared_in(app, &abs_path, options).await {
        Ok(answer) => {
            let declarations = SymbolFilter {
                pattern: args.symbol.as_deref(),
                substring: args.substring,
                include_kinds: include_kinds.as_deref(),
                exclude_kinds: exclude_kinds.as_deref(),
                structural: args.structural,
            }
            .outermost(answer.symbols);

            let total = declarations.len();
            let items: Vec<SymbolOutput> = declarations
                .iter()
                .take(limit)
                .map(|s| match source {
                    SymbolSource::Location => SymbolOutput::from_symbol(s, ctx.root()),
                    SymbolSource::Signature => SymbolOutput::from_symbol_signature(s, ctx.root()),
                    SymbolSource::Body => SymbolOutput::from_symbol(s, ctx.root())
                        .with_signature(extract_signature(s)),
                })
                .collect();

            let body_paths = SymbolOutput::body_paths(&items);
            ctx.print_source(
                FileSymbolsOutput {
                    backend: answer.backend,
                    symbols: Section::with_total(items, total),
                },
                &body_paths,
            );
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

/// A file's symbol list beside what produced it: an AST answer is flat and
/// carries no server-assigned kinds, so a caller reads `backend` before it
/// reads absence as structure.
#[derive(serde::Serialize)]
struct FileSymbolsOutput {
    backend: crate::cli::SymbolBackend,
    #[serde(flatten)]
    symbols: Section<SymbolOutput>,
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
