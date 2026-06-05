use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::Section;
use crate::infra::ast::{format_query_error, get_node_types, supported_languages};
use crate::models::symbol::Language;

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
    #[serde(flatten)]
    section: Section<NodeTypeOutput>,
}

#[derive(Serialize)]
struct NodeTypeOutput {
    category: &'static str,
    node_type: &'static str,
    example: &'static str,
    query: String,
}

/// Auto-wrap bare identifiers like `function_definition` so users don't
/// have to type the parentheses every time.
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

pub async fn execute_ast_search(
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
            OutputError::invalid("AST pattern cannot be empty").with_hint(
                "Example: function_definition or (function_definition). \
                 Use 'symora search nodes -l <lang>' to see available node types.",
            ),
        );
        return Ok(());
    }

    let normalized_pattern = normalize_ast_pattern(pattern);
    let pattern = &normalized_pattern;

    let lang = parse_language(language)?;
    let paths = path.unwrap_or_else(|| vec![app.root().to_path_buf()]);

    match app.ast.query(pattern, lang, &paths).await {
        Ok(matches) => {
            let total = matches.len();
            let limited: Vec<_> = if limit == 0 {
                matches
            } else {
                matches.into_iter().take(limit).collect()
            };
            let items = limited
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
                .collect();
            ctx.print_success(Section::with_total(items, total));
        }
        Err(crate::error::SearchError::InvalidPattern(e)) => {
            ctx.print_error(OutputError::invalid(format_query_error(lang, &e)));
        }
        Err(crate::error::SearchError::UnsupportedLanguage(l)) => {
            let supported: Vec<_> = supported_languages().iter().map(|l| l.lsp_id()).collect();
            ctx.print_error(
                OutputError::unsupported(format!("AST search not supported for {l:?}"))
                    .with_hint(format!("Supported languages: {}", supported.join(", "))),
            );
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

pub fn execute_list_nodes(app: &App, language: &str) -> Result<()> {
    let ctx = &app.output;
    let lang = parse_language(language)?;

    let nodes = get_node_types(lang);

    if nodes.is_empty() {
        let supported: Vec<_> = supported_languages().iter().map(|l| l.lsp_id()).collect();
        ctx.print_error(
            OutputError::unsupported(format!("AST search not supported for '{language}'"))
                .with_hint(format!("Supported languages: {}", supported.join(", "))),
        );
        return Ok(());
    }

    let response = NodesOutput {
        language: lang.lsp_id().to_string(),
        section: Section::new(
            nodes
                .iter()
                .map(|n| NodeTypeOutput {
                    category: n.category,
                    node_type: n.node_type,
                    example: n.example,
                    query: format!("({})", n.node_type),
                })
                .collect(),
        ),
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
