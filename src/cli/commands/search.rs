//! Search command implementation
//!
//! Provides four search modes:
//! - `symbols`: Ranked symbol search using SQLite LIKE (supports substring matching)
//! - `content`: Ranked content search using SQLite LIKE (supports substring matching)
//! - `ast`: Structural search using tree-sitter queries
//! - `nodes`: List available node types for AST search
//! - `index`: Manage search index (build/status/clear)

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
#[cfg(unix)]
use crate::daemon::DaemonClient;
use crate::infra::ast::{format_query_error, get_node_types, supported_languages};
use crate::models::symbol::Language;

#[derive(Args, Debug)]
pub struct SearchArgs {
    #[command(subcommand)]
    pub command: SearchCommand,
}

#[derive(Subcommand, Debug)]
pub enum SearchCommand {
    /// LIKE-based symbol search
    Symbols {
        /// Search query
        query: String,

        /// Symbol kind filter (function, class, struct, etc.)
        #[arg(short, long)]
        kind: Option<String>,

        /// Maximum results
        #[arg(short, long)]
        limit: Option<usize>,
    },

    /// LIKE-based content search
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

#[derive(Serialize)]
struct SymbolSearchResponse {
    count: usize,
    results: Vec<SymbolResultOutput>,
}

#[derive(Serialize)]
struct SymbolResultOutput {
    name: String,
    kind: String,
    file: String,
    line: u32,
    column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<String>,
    score: f64,
}

#[derive(Serialize)]
struct ContentSearchResponse {
    count: usize,
    results: Vec<ContentResultOutput>,
}

#[derive(Serialize)]
struct ContentResultOutput {
    file: String,
    line: u32,
    content: String,
    score: f64,
}

#[derive(Serialize)]
struct IndexStatusResponse {
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
    last_indexed: u64,
    is_indexing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<f32>,
}

#[derive(Serialize)]
struct IndexBuildResponse {
    status: String,
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
}

#[derive(Serialize)]
struct AstSearchResponse {
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
struct NodesResponse {
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

pub async fn execute(args: SearchArgs, app: &App) -> Result<()> {
    let cfg = app.config();

    match args.command {
        SearchCommand::Symbols { query, kind, limit } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_symbol_search(app, &query, kind.as_deref(), limit).await
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
    kind: Option<&str>,
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
        match client.search_symbols(query, Some(limit), kind).await {
            Ok(response) => {
                let count = response["count"].as_u64().unwrap_or(0) as usize;
                let results: Vec<SymbolResultOutput> = response["results"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|r| SymbolResultOutput {
                                name: r["name"].as_str().unwrap_or("").to_string(),
                                kind: r["kind"].as_str().unwrap_or("").to_string(),
                                file: ctx.relative_path(&std::path::PathBuf::from(
                                    r["file"].as_str().unwrap_or(""),
                                )),
                                line: r["line"].as_u64().unwrap_or(0) as u32,
                                column: r["column"].as_u64().unwrap_or(0) as u32,
                                container: r["container"]
                                    .as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(|s| s.to_string()),
                                score: r["score"].as_f64().unwrap_or(0.0),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                ctx.print_success_flat(SymbolSearchResponse { count, results });
            }
            Err(e) => {
                ctx.print_error(&format!("Symbol search failed: {}", e));
            }
        }
    }

    #[cfg(not(unix))]
    {
        ctx.print_error("Search requires daemon mode (Unix only)");
    }

    Ok(())
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
                let count = response["count"].as_u64().unwrap_or(0) as usize;
                let results: Vec<ContentResultOutput> = response["results"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|r| ContentResultOutput {
                                file: ctx.relative_path(&std::path::PathBuf::from(
                                    r["file"].as_str().unwrap_or(""),
                                )),
                                line: r["line"].as_u64().unwrap_or(0) as u32,
                                content: r["content"].as_str().unwrap_or("").to_string(),
                                score: r["score"].as_f64().unwrap_or(0.0),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                ctx.print_success_flat(ContentSearchResponse { count, results });
            }
            Err(e) => {
                ctx.print_error(&format!("Content search failed: {}", e));
            }
        }
    }

    #[cfg(not(unix))]
    {
        ctx.print_error("Search requires daemon mode (Unix only)");
    }

    Ok(())
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
                        let stats = &response["stats"];
                        ctx.print_success_flat(IndexBuildResponse {
                            status: "completed".to_string(),
                            file_count: stats["file_count"].as_u64().unwrap_or(0) as usize,
                            symbol_count: stats["symbol_count"].as_u64().unwrap_or(0) as usize,
                            content_line_count: stats["content_line_count"].as_u64().unwrap_or(0)
                                as usize,
                            index_size_bytes: stats["index_size_bytes"].as_u64().unwrap_or(0),
                        });
                    }
                    Err(e) => {
                        ctx.print_error(&format!("Index build failed: {}", e));
                    }
                }
            }
            IndexCommand::Status => match client.index_status().await {
                Ok(response) => {
                    ctx.print_success_flat(IndexStatusResponse {
                        file_count: response["file_count"].as_u64().unwrap_or(0) as usize,
                        symbol_count: response["symbol_count"].as_u64().unwrap_or(0) as usize,
                        content_line_count: response["content_line_count"].as_u64().unwrap_or(0)
                            as usize,
                        index_size_bytes: response["index_size_bytes"].as_u64().unwrap_or(0),
                        last_indexed: response["last_indexed"].as_u64().unwrap_or(0),
                        is_indexing: response["is_indexing"].as_bool().unwrap_or(false),
                        progress: response["progress"].as_f64().map(|p| p as f32),
                    });
                }
                Err(e) => {
                    ctx.print_error(&format!("Failed to get index status: {}", e));
                }
            },
            IndexCommand::Clear => match client.index_clear().await {
                Ok(_) => {
                    ctx.print_success_flat(serde_json::json!({ "cleared": true }));
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
            let response = AstSearchResponse {
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
            ctx.print_success_flat(response);
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

    let response = NodesResponse {
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

    ctx.print_success_flat(response);
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
