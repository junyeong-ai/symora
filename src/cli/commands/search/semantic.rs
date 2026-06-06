//! Natural-language semantic search across the project.
//!
//! Splits code files into ~30-line chunks, embeds each chunk with the
//! configured backend, and ranks chunks by cosine similarity to the query
//! embedding. Chunk vectors persist in `.symora/embeddings.db` keyed by
//! file mtime, so only files that changed since the last run are
//! re-embedded — the dominant cost is paid once and amortized across calls.
//!
//! The cache is a rebuildable optimization: if it can't be opened the
//! search still runs by embedding in memory, producing identical results
//! at the old cost. Builds without `--features embeddings` print an
//! actionable error instead of compiling out the subcommand.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::Section;
use crate::constants::defaults::{
    PACK_MAX_FILE_BYTES, SEMANTIC_CHUNK_LINES as CHUNK_LINES, SEMANTIC_MAX_CHUNKS as MAX_CHUNKS,
};
use crate::infra::file_filter::FileFilter;
use crate::models::lsp::IndexingDegradation;
use crate::models::symbol::Language;
use crate::services::embedding_cache::{CachedChunk, EmbeddingCache};
use crate::services::embeddings::{EmbeddingError, EmbeddingProvider, cosine, default_provider};

#[derive(Debug, Serialize)]
struct SemanticResultOutput {
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

    match semantic_search(app, provider.as_ref(), query, language, limit) {
        Ok(section) => ctx.print_success(section),
        Err(e) => ctx.print_error(OutputError::internal(e.to_string())),
    }
    Ok(())
}

/// Refresh the embedding corpus for the current query, then rank it. `count`
/// is the number of ranked candidates, so `truncated` honestly reflects the
/// agent is seeing the top of a larger ranking — the score column is the
/// relevance signal, so no opaque cutoff is imposed.
fn semantic_search(
    app: &App,
    provider: &dyn EmbeddingProvider,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<Section<SemanticResultOutput>> {
    let files = discover_files(app);
    if files.is_empty() {
        return Ok(Section::new(vec![]));
    }

    // The cache mirrors the whole code tree regardless of `--lang`; the flag
    // selects which language is ranked, with the corpus cap applied to that
    // language alone, so a large repo never starves one language's results.
    let language = language.map(Language::parse_or_default);
    let (corpus, capped) = build_corpus(app, provider, &files, language)?;
    if corpus.is_empty() {
        return Ok(Section::new(vec![]));
    }

    let query_vec = provider.embed_query(query)?;
    let mut scored: Vec<(f32, &(String, CachedChunk))> = corpus
        .iter()
        .map(|entry| (cosine(&query_vec, &entry.1.vector), entry))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let count = scored.len();
    let items: Vec<SemanticResultOutput> = scored
        .into_iter()
        .take(limit)
        .map(|(score, (file, chunk))| SemanticResultOutput {
            file: file.clone(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            score,
            snippet: chunk.snippet.clone(),
        })
        .collect();

    let section = Section::with_total(items, count);
    Ok(if capped {
        section.with_indexing(Some(IndexingDegradation::Capped))
    } else {
        section
    })
}

/// A discovered, in-budget source file: absolute path (for reading),
/// project-relative path (the cache key and output path), and mtime.
struct DiscoveredFile {
    path: PathBuf,
    rel: String,
    mtime: i64,
}

/// Every indexable code file in the project — language-agnostic, since the
/// cache is shared across `--lang` queries. Config/doc formats are excluded:
/// semantic search targets code.
fn discover_files(app: &App) -> Vec<DiscoveredFile> {
    let extensions: Vec<&str> = Language::all()
        .into_iter()
        .filter(|lang| !matches!(lang, Language::Markdown | Language::Yaml | Language::Toml))
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();

    let filter = FileFilter::with_gitignore(app.root());
    let mut files = filter.discover_files(&extensions);
    files.sort();

    files
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            if metadata.len() > PACK_MAX_FILE_BYTES {
                return None;
            }
            // Nanosecond resolution so two edits in the same wall-clock
            // second still register as a change and re-embed.
            let mtime = metadata
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos() as i64;
            let rel = app.output.relative_path(&path);
            Some(DiscoveredFile { path, rel, mtime })
        })
        .collect()
}

/// `true` when the ranked corpus is a prefix — more chunks matched than the
/// rank limit, so the score column reflects only the loaded window.
type Capped = bool;

/// Whether a file belongs to the requested language. No filter matches
/// everything; an unrecognized language (`Unknown`) matches no code file —
/// the same empty result content search returns, never a silent "rank
/// everything".
fn language_matches(file: &DiscoveredFile, language: Option<Language>) -> bool {
    match language {
        None => true,
        Some(lang) => Language::from_path(&file.path) == lang,
    }
}

/// Assemble the ranking corpus for `language`. The persistent cache mirrors
/// the whole code tree and is refreshed incrementally; ranking then loads
/// just the requested language, capped to [`MAX_CHUNKS`]. When the cache
/// can't open, embeds that language in memory instead.
fn build_corpus(
    app: &App,
    provider: &dyn EmbeddingProvider,
    files: &[DiscoveredFile],
    language: Option<Language>,
) -> Result<(Vec<(String, CachedChunk)>, Capped)> {
    match EmbeddingCache::open(app.root(), provider.model_id(), provider.dimension()) {
        Ok(cache) => {
            // Prune files that left disk so the cache stays an exact mirror;
            // it is language-agnostic, shared across every `--lang` query.
            let active: HashSet<String> = files.iter().map(|f| f.rel.clone()).collect();
            cache.prune(&active)?;
            refresh_cache(&cache, provider, files)?;
            let lang_id = language.map(|l| l.lsp_id());
            let (corpus, overflowed) = cache.load_corpus(lang_id, MAX_CHUNKS)?;
            Ok((corpus, overflowed))
        }
        Err(e) => {
            tracing::warn!("Embedding cache unavailable, embedding in memory: {e}");
            embed_in_memory(provider, files, language)
        }
    }
}

/// Re-embed every file whose mtime moved since it was last cached, keeping
/// the cache a complete, current mirror of the code tree. Cost is
/// incremental — only changed files run through the model.
fn refresh_cache(
    cache: &EmbeddingCache,
    provider: &dyn EmbeddingProvider,
    files: &[DiscoveredFile],
) -> Result<()> {
    for file in files {
        if cache.cached_mtime(&file.rel) == Some(file.mtime) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&file.path) else {
            continue;
        };
        let chunks = embed_chunks(provider, &chunk_file(&content))?;
        let language = Language::from_path(&file.path).lsp_id();
        cache.put_file(&file.rel, file.mtime, language, &chunks)?;
    }
    Ok(())
}

/// Fallback when the cache can't open: embed the requested language straight
/// into memory, capped at [`MAX_CHUNKS`] so a degraded run still bounds its
/// work.
fn embed_in_memory(
    provider: &dyn EmbeddingProvider,
    files: &[DiscoveredFile],
    language: Option<Language>,
) -> Result<(Vec<(String, CachedChunk)>, Capped)> {
    let mut corpus = Vec::new();
    for file in files {
        if !language_matches(file, language) {
            continue;
        }
        if corpus.len() >= MAX_CHUNKS {
            return Ok((corpus, true));
        }
        let Ok(content) = std::fs::read_to_string(&file.path) else {
            continue;
        };
        for chunk in embed_chunks(provider, &chunk_file(&content))? {
            if corpus.len() >= MAX_CHUNKS {
                return Ok((corpus, true));
            }
            corpus.push((file.rel.clone(), chunk));
        }
    }
    Ok((corpus, false))
}

/// A ~`CHUNK_LINES`-line window of a file: 1-indexed line span and text.
/// Blank windows are dropped so empty regions never reach the model.
fn chunk_file(content: &str) -> Vec<(u32, u32, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let mut chunks = Vec::new();
    for start in (0..lines.len()).step_by(CHUNK_LINES) {
        let end = (start + CHUNK_LINES).min(lines.len());
        let text = lines[start..end].join("\n");
        if text.trim().is_empty() {
            continue;
        }
        chunks.push((start as u32 + 1, end as u32, text));
    }
    chunks
}

/// Embed a file's chunks in one batch, pairing each vector with its span
/// and a display snippet.
fn embed_chunks(
    provider: &dyn EmbeddingProvider,
    chunks: &[(u32, u32, String)],
) -> Result<Vec<CachedChunk>, EmbeddingError> {
    if chunks.is_empty() {
        return Ok(Vec::new());
    }
    let texts: Vec<String> = chunks.iter().map(|(_, _, text)| text.clone()).collect();
    let vectors = provider.embed_batch(&texts)?;
    Ok(chunks
        .iter()
        .zip(vectors)
        .map(|((start, end, text), vector)| CachedChunk {
            start_line: *start,
            end_line: *end,
            snippet: snippet_preview(text),
            vector,
        })
        .collect())
}

/// First few lines of a chunk, hard-capped on a char boundary so multibyte
/// source (non-ASCII identifiers, comments, string literals) can never
/// split mid-character.
fn snippet_preview(text: &str) -> String {
    const MAX_LEN: usize = 240;
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
        if out.len() >= MAX_LEN {
            break;
        }
    }
    if out.len() > MAX_LEN {
        let end = (0..=MAX_LEN)
            .rev()
            .find(|&i| out.is_char_boundary(i))
            .unwrap_or(0);
        out.truncate(end);
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Deterministic, model-free provider so the incremental-refresh logic
    /// can be tested without loading an ONNX model. It records how many
    /// chunks it was asked to embed, which is the signal these tests assert
    /// on (an unchanged file must not reach the provider again).
    struct CountingProvider {
        embedded: Arc<AtomicUsize>,
    }

    impl EmbeddingProvider for CountingProvider {
        fn model_id(&self) -> &str {
            "counting"
        }
        fn dimension(&self) -> usize {
            3
        }
        fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            self.embedded.fetch_add(texts.len(), Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|t| vec![t.len() as f32, 0.0, 1.0])
                .collect())
        }
    }

    fn discovered(dir: &std::path::Path, name: &str, body: &str, mtime: i64) -> DiscoveredFile {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        DiscoveredFile {
            path,
            rel: name.to_string(),
            mtime,
        }
    }

    #[test]
    fn refresh_embeds_once_then_reuses_until_mtime_moves() {
        let dir = tempfile::tempdir().unwrap();
        let embedded = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            embedded: Arc::clone(&embedded),
        };
        let cache =
            EmbeddingCache::open(dir.path(), provider.model_id(), provider.dimension()).unwrap();

        let files = vec![
            discovered(dir.path(), "a.rs", "fn a() {}\n", 1),
            discovered(dir.path(), "b.rs", "fn b() {}\n", 1),
        ];

        // First pass embeds both files.
        refresh_cache(&cache, &provider, &files).unwrap();
        assert_eq!(cache.total_chunks().unwrap(), 2);
        let after_first = embedded.load(Ordering::SeqCst);
        assert_eq!(after_first, 2);

        // Same mtimes: nothing is re-embedded.
        refresh_cache(&cache, &provider, &files).unwrap();
        assert_eq!(embedded.load(Ordering::SeqCst), after_first);

        // Bumping one file's mtime re-embeds only that file.
        let moved = vec![
            discovered(dir.path(), "a.rs", "fn a() {}\n", 2),
            DiscoveredFile {
                path: dir.path().join("b.rs"),
                rel: "b.rs".to_string(),
                mtime: 1,
            },
        ];
        refresh_cache(&cache, &provider, &moved).unwrap();
        assert_eq!(embedded.load(Ordering::SeqCst), after_first + 1);
        assert_eq!(cache.total_chunks().unwrap(), 2);
    }

    #[test]
    fn refresh_reembeds_a_changed_file_regardless_of_corpus_size() {
        // A large cache must never block re-embedding a file whose mtime
        // moved, or semantic search serves stale vectors forever.
        let dir = tempfile::tempdir().unwrap();
        let embedded = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            embedded: Arc::clone(&embedded),
        };
        let cache =
            EmbeddingCache::open(dir.path(), provider.model_id(), provider.dimension()).unwrap();

        for i in 0..MAX_CHUNKS {
            cache
                .put_file(
                    &format!("seed{i}.rs"),
                    1,
                    "rust",
                    &[CachedChunk {
                        start_line: 1,
                        end_line: 1,
                        snippet: String::new(),
                        vector: vec![0.0, 0.0, 0.0],
                    }],
                )
                .unwrap();
        }
        // One seeded file exists on disk and has changed since it was cached.
        let changed = discovered(dir.path(), "seed0.rs", "fn changed() {}\n", 2);
        refresh_cache(&cache, &provider, &[changed]).unwrap();
        assert!(
            embedded.load(Ordering::SeqCst) > 0,
            "a changed file must re-embed no matter how large the cache is"
        );
    }

    #[test]
    fn language_matches_absent_valid_and_unknown() {
        let dir = std::path::Path::new("/proj");
        let rs = DiscoveredFile {
            path: dir.join("a.rs"),
            rel: "a.rs".into(),
            mtime: 1,
        };
        // No filter matches any file.
        assert!(language_matches(&rs, None));
        // A valid language matches its own files and rejects others.
        assert!(language_matches(&rs, Some(Language::Rust)));
        assert!(!language_matches(&rs, Some(Language::Python)));
        // An unrecognized language parses to Unknown, which matches no code
        // file — the same empty result content search returns.
        assert!(!language_matches(
            &rs,
            Some(Language::parse_or_default("cobol"))
        ));
    }

    #[test]
    fn snippet_preview_never_splits_a_multibyte_char() {
        // A line of multibyte chars longer than the cap must truncate on a
        // char boundary (a byte-index truncate would panic here).
        let text = "café_변수_ ".repeat(60).chars().collect::<String>();
        let preview = snippet_preview(&text);
        assert!(preview.is_char_boundary(preview.len()));
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn short_snippet_is_returned_whole() {
        assert_eq!(snippet_preview("fn main() {}"), "fn main() {}");
    }

    #[test]
    fn chunk_file_spans_are_one_indexed_and_skip_blank_windows() {
        let content = (1..=45)
            .map(|n| if (16..=30).contains(&n) { "" } else { "code" })
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_file(&content);
        // 45 lines, 30-line windows → [1..30], [31..45]; the middle of the
        // first window has content so it survives, the second is all code.
        assert_eq!(chunks[0].0, 1);
        assert_eq!(chunks[0].1, 30);
        assert_eq!(chunks[1].0, 31);
        assert_eq!(chunks[1].1, 45);
    }

    #[test]
    fn chunk_file_drops_fully_blank_windows() {
        let content = "\n".repeat(30) + "real code";
        let chunks = chunk_file(&content);
        // The first all-blank window is dropped; only the final line remains.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, 31);
    }
}
