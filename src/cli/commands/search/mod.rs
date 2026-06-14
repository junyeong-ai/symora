use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;

mod ast;
mod common;
mod content;
mod index;
mod semantic;
mod symbols;

pub use ast::{execute_ast_search, execute_list_nodes};
pub use content::execute_content_search;
pub use index::execute_index_command;
pub use semantic::execute_semantic_search;
pub use symbols::execute_symbol_search;

#[derive(Args, Debug)]
#[command(
    after_long_help = "Use `search` when you have a rough name or phrase but not an exact file.\n\
                       Use `search symbols` for approximate workspace discovery.\n\
                       Use `symbols` once you already know the file or a symbol path.\n\
                       Typical flow:\n  \
                       1. `symora search symbols auth`\n  \
                       2. `symora map file <match>`\n  \
                       3. `symora symbols <file>` or `symora symbols --symbol <path>`\n  \
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

        /// Language filter for LSP workspace search
        #[arg(short, long = "lang")]
        language: Option<String>,

        /// Symbol kind filter (function, class, struct, etc.)
        #[arg(short, long)]
        kind: Option<String>,

        /// Force live LSP workspace-symbol search (bypass the index)
        #[arg(long)]
        workspace_symbols: bool,

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

    /// Natural-language semantic search (requires the 'embeddings' feature).
    Semantic {
        /// Natural-language query, e.g. "where is the retry logic".
        query: String,

        /// Optional language filter
        #[arg(short, long = "lang")]
        language: Option<String>,

        /// Maximum results
        #[arg(long)]
        limit: Option<usize>,
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
            language,
            kind,
            workspace_symbols,
            limit,
        } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_symbol_search(
                app,
                &query,
                language.as_deref(),
                kind.as_deref(),
                workspace_symbols,
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
        SearchCommand::Semantic {
            query,
            language,
            limit,
        } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_semantic_search(app, &query, language.as_deref(), limit).await
        }
    }
}
