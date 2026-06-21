use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::Section;
use crate::error::StoreError;
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::Language;

use super::common::is_code_language;

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

    let language_filter = language.map(Language::parse_or_default);
    match app
        .store
        .search_content(query, limit, language_filter)
        .await
    {
        Ok(page) => {
            let items = page
                .rows
                .into_iter()
                .map(|r| ContentResultOutput {
                    file: ctx.relative_path(&r.file),
                    line: r.line,
                    content: r.content,
                    backend: Some("index".to_string()),
                    score: r.score,
                })
                .collect();
            ctx.print_success(
                finish_content_search(items, page.total, query, language, limit)
                    .with_stale(page.stale),
            );
        }
        // No index yet: a one-shot filesystem scan keeps content search
        // working with zero setup. Any other error is real and surfaced.
        Err(StoreError::NotInitialized) => {
            let parsed = fallback_content_search(app, query, language, limit).await?;
            ctx.print_success(parsed);
        }
        Err(e) => ctx.print_error(OutputError::internal(e.to_string())),
    }

    Ok(())
}

/// Final shaping shared by the index and scan paths: prioritize code-file
/// matches, cap emission at `limit`, and derive `truncated`/hints from the
/// exact match count — never from limit saturation.
fn finish_content_search(
    candidates: Vec<ContentResultOutput>,
    count: usize,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Section<ContentResultOutput> {
    let count = count.max(candidates.len());
    let items = prioritize_code_content_results(candidates, language, limit);

    let truncated = items.len() < count;
    let hints = content_search_hints(query, language, truncated);
    let next_commands = content_search_next_commands(&items, language);
    Section::with_total(items, count)
        .with_hints(hints)
        .with_next_commands(next_commands)
}

async fn fallback_content_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<Section<ContentResultOutput>> {
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

    let filter = FileFilter::new(app.root());
    let mut files = filter.discover_files(&extensions);
    files.sort();

    let q = query.to_ascii_lowercase();
    // Storage is capped to bound memory; the scan itself runs to the end
    // so `count` stays an exact total, never a silent lower bound.
    let storage_cap = limit * 8;
    let mut total = 0usize;
    let mut results = Vec::new();

    for file in files {
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

            total += 1;
            if results.len() < storage_cap {
                results.push(ContentResultOutput {
                    file: app.output.relative_path(&file),
                    line: idx as u32 + 1,
                    content: line.to_string(),
                    backend: Some("scan".to_string()),
                    score,
                });
            }
        }
    }

    Ok(finish_content_search(
        results, total, query, language, limit,
    ))
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

/// Score by the match's character position within the trimmed line,
/// mirroring the SQL ladder in `build_content_search_query` (whose `INSTR`
/// is character-based) so the scan and index paths rank identically even
/// with multibyte text. `query` is already lowercased by the caller.
fn score_content_line(query: &str, line: &str) -> f64 {
    let trimmed = line.trim().to_ascii_lowercase();
    let Some(byte_pos) = trimmed.find(query) else {
        return 0.0;
    };
    match trimmed[..byte_pos].chars().count() {
        0 => 1.0,
        pos if pos < 8 => 0.8,
        pos if pos < 32 => 0.6,
        _ => 0.4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(file: &str, line: u32) -> ContentResultOutput {
        ContentResultOutput {
            file: file.to_string(),
            line,
            content: "async fn run() {}".to_string(),
            backend: Some("scan".to_string()),
            score: 1.0,
        }
    }

    #[test]
    fn finish_content_search_derives_truncation_from_exact_count() {
        let candidates = vec![result("src/a.rs", 1), result("src/b.rs", 2)];
        let section = finish_content_search(candidates, 10, "run", None, 2);

        assert_eq!(section.count, 10);
        assert_eq!(section.showing, 2);
        assert!(section.truncated);
    }

    #[test]
    fn finish_content_search_complete_results_are_not_truncated() {
        let candidates = vec![result("src/a.rs", 1)];
        let section = finish_content_search(candidates, 1, "run", None, 10);

        assert_eq!(section.count, 1);
        assert_eq!(section.showing, 1);
        assert!(!section.truncated);
    }
}
