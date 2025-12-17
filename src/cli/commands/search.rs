//! Search command implementation
//!
//! Provides three search modes:
//! - `text`: Fast regex search using ripgrep
//! - `ast`: Structural search using tree-sitter queries
//! - `nodes`: List available node types for AST search

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::app::App;
use crate::infra::ast::{format_query_error, get_node_types, supported_languages};
use crate::models::symbol::Language;

#[derive(Args, Debug)]
pub struct SearchArgs {
    #[command(subcommand)]
    pub command: SearchCommand,
}

#[derive(Subcommand, Debug)]
pub enum SearchCommand {
    /// Fast regex search using ripgrep
    Text {
        /// Regex pattern to search for
        pattern: String,

        /// Search path (defaults to project root)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// File type filter (rust, ts, py, go, java, etc.)
        #[arg(short = 't', long = "type")]
        file_type: Option<String>,

        /// Case insensitive search
        #[arg(short, long)]
        ignore_case: bool,

        /// Match whole words only
        #[arg(short, long)]
        word: bool,

        /// Maximum results (0 = unlimited, default from config)
        #[arg(long)]
        limit: Option<usize>,

        /// Context lines before and after match
        #[arg(short = 'C', long, default_value = "0")]
        context: u32,
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
}

// === Response Types ===

#[derive(Serialize)]
struct TextSearchResponse {
    count: usize,
    matches: Vec<TextMatchOutput>,
}

#[derive(Serialize)]
struct TextMatchOutput {
    file: String,
    line: u32,
    column: u32,
    text: String,
    #[serde(rename = "match")]
    matched: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_before: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_after: Option<Vec<String>>,
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

// === ripgrep JSON types ===

#[derive(Deserialize)]
struct RgMessage {
    #[serde(rename = "type")]
    msg_type: String,
    data: Option<RgData>,
}

#[derive(Deserialize)]
struct RgData {
    path: Option<RgPath>,
    lines: Option<RgLines>,
    line_number: Option<u32>,
    submatches: Option<Vec<RgSubmatch>>,
}

#[derive(Deserialize)]
struct RgPath {
    text: String,
}

#[derive(Deserialize)]
struct RgLines {
    text: String,
}

#[derive(Deserialize)]
struct RgSubmatch {
    #[serde(rename = "match")]
    matched: RgMatch,
    start: u32,
}

#[derive(Deserialize)]
struct RgMatch {
    text: String,
}

pub async fn execute(args: SearchArgs, app: &App) -> Result<()> {
    let cfg = app.config();

    match args.command {
        SearchCommand::Text {
            pattern,
            path,
            file_type,
            ignore_case,
            word,
            limit,
            context,
        } => {
            let limit = limit.unwrap_or(cfg.search.limit);
            execute_text_search(
                app,
                &pattern,
                path,
                file_type,
                ignore_case,
                word,
                limit,
                context,
            )
            .await
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
    }
}

/// Validate search pattern
fn validate_pattern(pattern: &str) -> Result<&str, &'static str> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err("Search pattern cannot be empty");
    }
    // Check for whitespace-only patterns (after trim, so this catches original whitespace)
    if pattern.chars().all(|c| c.is_whitespace()) {
        return Err("Search pattern cannot contain only whitespace");
    }
    Ok(trimmed)
}

async fn execute_text_search(
    app: &App,
    pattern: &str,
    path: Option<PathBuf>,
    file_type: Option<String>,
    ignore_case: bool,
    word: bool,
    limit: usize,
    context: u32,
) -> Result<()> {
    let ctx = &app.output;

    // Validate and normalize pattern
    let pattern = match validate_pattern(pattern) {
        Ok(p) => p,
        Err(msg) => {
            ctx.print_error(msg);
            return Ok(());
        }
    };

    let search_path = path.unwrap_or_else(|| app.root().to_path_buf());

    // Build ripgrep command
    // Note: ripgrep's --max-count is per-file, not total. We apply limit during parsing.
    let mut cmd = Command::new("rg");
    cmd.arg("--json");

    if ignore_case {
        cmd.arg("-i");
    }

    if word {
        cmd.arg("-w");
    }

    if context > 0 {
        cmd.arg("-C").arg(context.to_string());
    }

    // Validate and map file type to ripgrep type
    if let Some(ref ft) = file_type {
        match validate_file_type(ft) {
            Ok(rg_type) => {
                cmd.arg("-t").arg(rg_type);
            }
            Err(err_msg) => {
                ctx.print_error(&err_msg);
                return Ok(());
            }
        }
    }

    cmd.arg(pattern).arg(&search_path);

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd.output().await;

    match output {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(-1);

            match exit_code {
                0 => {
                    // Success - matches found, parse JSON output
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let matches = parse_ripgrep_output(&stdout, ctx.root(), limit);
                    let response = TextSearchResponse {
                        count: matches.len(),
                        matches,
                    };
                    ctx.print_success_flat(response);
                }
                1 => {
                    // No matches found - valid success with empty results
                    let response = TextSearchResponse {
                        count: 0,
                        matches: vec![],
                    };
                    ctx.print_success_flat(response);
                }
                2 => {
                    // Error - invalid regex or other ripgrep error
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stderr = stderr.trim();
                    if stderr.is_empty() {
                        ctx.print_error("Search failed: invalid pattern or configuration");
                    } else {
                        // Extract just the error message (strip "rg: " prefix if present)
                        let error_msg = stderr
                            .lines()
                            .find(|l| l.contains("error:") || l.starts_with("rg:"))
                            .map(|l| l.trim_start_matches("rg: "))
                            .unwrap_or(stderr);
                        ctx.print_error(&format!("Search failed: {}", error_msg));
                    }
                }
                _ => {
                    // Other exit codes - check stderr for context
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if stderr.contains("not found") || stderr.contains("No such file") {
                        ctx.print_error(
                            "ripgrep (rg) not found. Install: brew install ripgrep (macOS) or cargo install ripgrep",
                        );
                    } else {
                        ctx.print_error(&format!(
                            "Search failed (exit code {}): {}",
                            exit_code,
                            stderr.trim()
                        ));
                    }
                }
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                ctx.print_error(
                    "ripgrep (rg) not found. Install: brew install ripgrep (macOS) or cargo install ripgrep",
                );
            } else {
                ctx.print_error(&format!("Failed to execute ripgrep: {}", e));
            }
        }
    }

    Ok(())
}

fn parse_ripgrep_output(
    stdout: &str,
    root: &std::path::Path,
    limit: usize,
) -> Vec<TextMatchOutput> {
    let capacity = if limit == 0 { 1000 } else { limit.min(1000) };
    let mut matches = Vec::with_capacity(capacity);

    for line in stdout.lines() {
        if limit > 0 && matches.len() >= limit {
            break;
        }

        if line.is_empty() {
            continue;
        }

        if let Ok(msg) = serde_json::from_str::<RgMessage>(line)
            && msg.msg_type == "match"
            && let Some(data) = msg.data
        {
            let file = data
                .path
                .map(|p| {
                    let path = std::path::Path::new(&p.text);
                    if let Ok(rel) = path.strip_prefix(root) {
                        rel.display().to_string()
                    } else {
                        p.text
                    }
                })
                .unwrap_or_default();

            let text = data
                .lines
                .map(|l| l.text.trim_end().to_string())
                .unwrap_or_default();

            let line_number = data.line_number.unwrap_or(0);

            let (column, matched) = data
                .submatches
                .and_then(|subs| subs.into_iter().next())
                .map(|sub| (sub.start + 1, sub.matched.text))
                .unwrap_or((1, String::new()));

            matches.push(TextMatchOutput {
                file,
                line: line_number,
                column,
                text,
                matched,
                context_before: None,
                context_after: None,
            });
        }
    }

    matches
}

/// Supported file types for text search
const SUPPORTED_FILE_TYPES: &[(&str, &str)] = &[
    ("rust", "rust"),
    ("rs", "rust"),
    ("typescript", "ts"),
    ("ts", "ts"),
    ("tsx", "ts"),
    ("javascript", "js"),
    ("js", "js"),
    ("jsx", "js"),
    ("python", "py"),
    ("py", "py"),
    ("go", "go"),
    ("golang", "go"),
    ("java", "java"),
    ("kotlin", "kotlin"),
    ("kt", "kotlin"),
    ("c", "c"),
    ("cpp", "cpp"),
    ("c++", "cpp"),
    ("csharp", "csharp"),
    ("cs", "csharp"),
    ("ruby", "ruby"),
    ("rb", "ruby"),
    ("php", "php"),
    ("lua", "lua"),
    ("bash", "sh"),
    ("sh", "sh"),
    ("json", "json"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("toml", "toml"),
    ("md", "md"),
    ("markdown", "md"),
    ("html", "html"),
    ("css", "css"),
    ("sql", "sql"),
    ("swift", "swift"),
    ("scala", "scala"),
    ("elixir", "elixir"),
    ("haskell", "haskell"),
    ("hs", "haskell"),
];

fn map_file_type(ft: &str) -> Option<&'static str> {
    let lower = ft.to_lowercase();
    SUPPORTED_FILE_TYPES
        .iter()
        .find(|(alias, _)| *alias == lower.as_str())
        .map(|(_, rg_type)| *rg_type)
}

fn validate_file_type(ft: &str) -> Result<&'static str, String> {
    match map_file_type(ft) {
        Some(rg_type) => Ok(rg_type),
        None => {
            let valid_types: Vec<_> = SUPPORTED_FILE_TYPES
                .iter()
                .map(|(alias, _)| *alias)
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            Err(format!(
                "Unknown file type: '{}'. Supported: {}",
                ft,
                valid_types.join(", ")
            ))
        }
    }
}

/// Normalize AST pattern by auto-wrapping simple node types with parentheses.
/// Simple node types contain only alphanumeric characters and underscores.
fn normalize_ast_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim();

    // Already wrapped in parentheses - return as-is
    if trimmed.starts_with('(') {
        return trimmed.to_string();
    }

    // Check if it's a simple node type (alphanumeric + underscore only)
    let is_simple = !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');

    if is_simple {
        format!("({})", trimmed)
    } else {
        // Complex pattern that doesn't start with '(' - likely an error
        // Return as-is and let tree-sitter provide the error message
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

    // Validate pattern (trim and check for empty/whitespace-only)
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
            // Use enhanced error message with hints
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
