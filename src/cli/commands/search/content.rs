use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::OutputError;
#[cfg(unix)]
use crate::daemon::DaemonClient;
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::Language;

use super::common::is_code_language;

#[derive(Serialize, Deserialize)]
pub(super) struct ContentSearchOutput {
    pub count: usize,
    #[serde(default)]
    pub showing: usize,
    #[serde(alias = "results")]
    pub items: Vec<ContentResultOutput>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_commands: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ContentResultOutput {
    pub file: String,
    pub line: u32,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub score: f64,
}

pub async fn execute_content_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;

    let query = query.trim();
    if query.is_empty() {
        ctx.print_error(OutputError::invalid("Search query cannot be empty"));
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
                    ctx.print_error(e);
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

#[cfg(test)]
mod tests {
    use super::*;

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
