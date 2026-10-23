use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::response::SymbolSource;

mod ast;
mod common;
mod content;
mod index;
mod symbols;

pub use ast::{execute_ast_search, execute_list_nodes};
pub use content::execute_content_search;
pub use index::execute_index_command;
pub use symbols::{SymbolQuery, execute_symbol_search};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Use `search` when you have a rough name or phrase but not an exact file.\n\
                       Use `search symbols` for approximate workspace discovery.\n\
                       Use `symbols` once you already know the file.\n\
                       Typical flow:\n  \
                       1. `symora search symbols auth`\n  \
                       2. `symora map file <match>`\n  \
                       3. `symora symbols <file>` or `symora search symbols <Class/method> --body`\n  \
                       4. `symora refs <loc>`\n"
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

        /// Search only symbols declared in these files or directories
        #[arg(short, long)]
        path: Option<Vec<PathBuf>>,

        /// Language filter
        #[arg(short, long = "lang")]
        language: Option<String>,

        /// Symbol kind filter (function, class, struct, etc.)
        #[arg(short, long)]
        kind: Option<String>,

        /// Force live LSP workspace-symbol search (bypass the index)
        #[arg(long)]
        workspace_symbols: bool,

        /// Attach each result's complete definition
        #[arg(long, conflicts_with = "signature")]
        body: bool,

        /// Attach each result's signature
        #[arg(long, conflicts_with = "body")]
        signature: bool,

        /// Maximum results
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Fast content lookup by keyword or phrase
    Content {
        /// Search query
        query: String,

        /// Search only these files or directories, including hidden paths; respects ignore rules
        #[arg(short, long)]
        path: Option<Vec<PathBuf>>,

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

        /// Search only these files or directories, including hidden paths; respects ignore rules
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

pub async fn execute(args: SearchArgs, app: &App) -> Result<()> {
    let cfg = app.config();

    match args.command {
        SearchCommand::Symbols {
            query,
            path,
            language,
            kind,
            workspace_symbols,
            body,
            signature,
            limit,
        } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_symbol_search(
                app,
                SymbolQuery {
                    query: &query,
                    language: language.as_deref(),
                    kind: kind.as_deref(),
                    paths: path.as_deref(),
                    workspace_symbols,
                    source: SymbolSource::new(body, signature),
                    limit,
                },
            )
            .await
        }
        SearchCommand::Content {
            query,
            path,
            language,
            limit,
        } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_content_search(app, &query, language.as_deref(), path.as_deref(), limit).await
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
