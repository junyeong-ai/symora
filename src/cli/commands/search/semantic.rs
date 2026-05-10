//! Natural-language semantic search across the project.
//!
//! Walks code files, splits them into ~30-line chunks, embeds each chunk
//! with the configured backend, and ranks chunks by cosine similarity to
//! the query embedding. The first invocation pays the model-load cost
//! (~1-2s) but everything stays in-process, so a follow-up call inside
//! the same daemon session is fast.
//!
//! Builds without `--features embeddings` print an actionable error
//! instead of compiling out the subcommand — agents discover the
//! capability through `--help` and learn how to enable it.

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::constants::defaults::{
    PACK_MAX_FILE_BYTES, SEMANTIC_CHUNK_LINES as CHUNK_LINES, SEMANTIC_MAX_CHUNKS as MAX_CHUNKS,
};
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::Language;
use crate::services::embeddings::{EmbeddingError, EmbeddingProvider, cosine, default_provider};

#[derive(Debug, Serialize)]
struct SemanticSearchOutput {
    query: String,
    model: String,
    embedded_chunks: usize,
    items: Vec<SemanticHitOutput>,
}

#[derive(Debug, Serialize)]
struct SemanticHitOutput {
    file: String,
    start_line: u32,
    end_line: u32,
    score: f32,
    snippet: String,
}

pub async fn execute_semantic_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;
    if query.trim().is_empty() {
        ctx.print_error(OutputError::invalid(
            "Semantic search query cannot be empty",
        ));
        return Ok(());
    }

    let provider = match default_provider() {
        Ok(p) => p,
        Err(EmbeddingError::FeatureDisabled) => {
            ctx.print_error(
                OutputError::unsupported("Semantic search requires the 'embeddings' feature")
                    .with_hint("Reinstall with: cargo install symora --features embeddings"),
            );
            return Ok(());
        }
        Err(e) => {
            ctx.print_error(OutputError::internal(e.to_string()));
            return Ok(());
        }
    };

    let chunks = collect_chunks(app, language);
    if chunks.is_empty() {
        ctx.print_success(SemanticSearchOutput {
            query: query.to_string(),
            model: provider.model_id().to_string(),
            embedded_chunks: 0,
            items: vec![],
        });
        return Ok(());
    }

    rank_and_emit(ctx, query, limit, provider.as_ref(), chunks);
    Ok(())
}

fn rank_and_emit(
    ctx: &crate::cli::OutputContext,
    query: &str,
    limit: usize,
    provider: &dyn EmbeddingProvider,
    chunks: Vec<Chunk>,
) {
    let query_vec = match provider.embed_query(query) {
        Ok(v) => v,
        Err(e) => {
            ctx.print_error(OutputError::internal(e.to_string()));
            return;
        }
    };

    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let vectors = match provider.embed_batch(&texts) {
        Ok(v) => v,
        Err(e) => {
            ctx.print_error(OutputError::internal(e.to_string()));
            return;
        }
    };

    let mut scored: Vec<(f32, &Chunk)> = chunks
        .iter()
        .zip(vectors.iter())
        .map(|(chunk, vec)| (cosine(&query_vec, vec), chunk))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let items = scored
        .into_iter()
        .take(limit)
        .map(|(score, chunk)| SemanticHitOutput {
            file: ctx.relative_path(&chunk.file),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            score,
            snippet: snippet_preview(&chunk.text),
        })
        .collect();

    ctx.print_success(SemanticSearchOutput {
        query: query.to_string(),
        model: provider.model_id().to_string(),
        embedded_chunks: chunks.len(),
        items,
    });
}

#[derive(Debug, Clone)]
struct Chunk {
    file: std::path::PathBuf,
    start_line: u32,
    end_line: u32,
    text: String,
}

fn collect_chunks(app: &App, language: Option<&str>) -> Vec<Chunk> {
    let language_filter = language.map(Language::parse_or_default);
    let extensions: Vec<&str> = match language_filter {
        Some(Language::Unknown) | None => Language::all()
            .into_iter()
            .filter(|lang| !matches!(lang, Language::Markdown | Language::Yaml | Language::Toml))
            .flat_map(|lang| lang.extensions().iter().copied())
            .collect(),
        Some(lang) => lang.extensions().to_vec(),
    };

    let filter = FileFilter::with_gitignore(app.root());
    let files = filter.discover_files(&extensions);

    let mut chunks = Vec::new();
    for file in files {
        if chunks.len() >= MAX_CHUNKS {
            break;
        }
        let Ok(metadata) = std::fs::metadata(&file) else {
            continue;
        };
        if metadata.len() > PACK_MAX_FILE_BYTES {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for window_start in (0..lines.len()).step_by(CHUNK_LINES) {
            if chunks.len() >= MAX_CHUNKS {
                break;
            }
            let end = (window_start + CHUNK_LINES).min(lines.len());
            let text = lines[window_start..end].join("\n");
            if text.trim().is_empty() {
                continue;
            }
            chunks.push(Chunk {
                file: file.clone(),
                start_line: window_start as u32 + 1,
                end_line: end as u32,
                text,
            });
        }
    }
    chunks
}

fn snippet_preview(text: &str) -> String {
    const MAX_LEN: usize = 240;
    let mut snippet = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            snippet.push('\n');
        }
        snippet.push_str(line);
        if snippet.len() >= MAX_LEN {
            break;
        }
    }
    if snippet.len() > MAX_LEN {
        snippet.truncate(MAX_LEN);
        snippet.push('…');
    }
    snippet
}
