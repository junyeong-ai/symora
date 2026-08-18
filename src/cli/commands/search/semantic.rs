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
//! at the full un-amortized cost. Builds without `--features embeddings`
//! print an actionable error instead of compiling out the subcommand.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::Section;
use crate::constants::defaults::{PACK_MAX_FILE_BYTES, SEMANTIC_CHUNK_LINES as CHUNK_LINES};
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::Language;

use crate::cli::response::disclosure::{LowerBound, relative_unread_paths, with_lower_bounds};
use crate::cli::symbol_discovery::is_code_language;
use crate::services::embedding_cache::{CachedChunk, EmbeddingCache, RankedChunk, TopK};
use crate::services::embeddings::{EmbeddingError, EmbeddingProvider, cosine, default_provider};
use crate::services::store::UnreadPath;

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

    if language.map(Language::parse_or_default) == Some(Language::Unknown) {
        ctx.print_error(
            OutputError::invalid(format!(
                "Unknown language: {}",
                language.unwrap_or_default()
            ))
            .with_hint("Run 'symora doctor' to see supported languages."),
        );
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
    let language = language.map(Language::parse_or_default);
    let corpus = discover_files(app);
    // Discovery settles what the walk could not read; embedding below adds
    // what it could not read afterwards, and the two finish as one list.
    // A ranking narrowed to one language is not shortened by a file of
    // another: the filter excluded it before the read ever failed. A path the
    // walk could not ENTER names no language, so it could have held this one.
    let mut unread_paths: Vec<String> = relative_unread_paths(&app.output, &corpus.unread_paths)
        .into_iter()
        .filter(|unread| match (language, unread.is_file) {
            (Some(lang), true) => Language::from_path(std::path::Path::new(&unread.path)) == lang,
            _ => true,
        })
        .map(|unread| unread.path)
        .collect();
    let files = corpus.files;
    if files.is_empty() {
        return Ok(bounded_section(Section::new(vec![]), unread_paths));
    }

    let query_vec = provider.embed_query(query)?;
    let score = |vector: &[f32]| cosine(&query_vec, vector);

    // The cache is shared across queries; `--lang` selects which language is
    // embedded and ranked. Ranking streams every candidate through a bounded
    // top-`limit` heap, so the best matches are found without ever holding the
    // whole corpus in memory.
    let (ranked, total) =
        match EmbeddingCache::open(app.root(), provider.model_id(), provider.dimension()) {
            Ok(cache) => {
                let active: HashSet<String> = files.iter().map(|f| f.rel.clone()).collect();
                // Prune DELETES what the walk did not find, which only means
                // "gone" when the walk saw everything — a path it could not
                // read is not absent, and dropping its embedding would throw
                // away work a later, complete walk has to redo. This holds on
                // every run because a read failure re-counts on every run.
                // Ranking is filtered by the same set unconditionally: what
                // may not be deleted on an incomplete walk still may not be
                // ranked, or a file that changed behind an unreadable path
                // would keep scoring against content it no longer has.
                if unread_paths.is_empty() {
                    cache.prune(&active)?;
                }
                unread_paths.extend(refresh_cache(&cache, provider, &files, language)?);
                let lang_id = language.map(|l| l.lsp_id());
                cache.rank_top(lang_id, &active, limit, score)?
            }
            Err(e) => {
                tracing::warn!("Embedding cache unavailable, embedding in memory: {e}");
                rank_in_memory(provider, &files, language, limit, score, &mut unread_paths)?
            }
        };

    let items: Vec<SemanticResultOutput> = ranked
        .into_iter()
        .map(|r| SemanticResultOutput {
            file: r.file,
            start_line: r.start_line,
            end_line: r.end_line,
            score: r.score,
            snippet: r.snippet,
        })
        .collect();

    // `total` is the full candidate count, so `truncated` honestly says the
    // agent is seeing the top `--limit` of a larger relevance ranking — over
    // the corpus that could be read, which `incomplete` qualifies.
    Ok(bounded_section(
        Section::with_total(items, total),
        unread_paths,
    ))
}

/// Publish a ranking over a corpus that may be short of the project, naming
/// the reason. Both exits take it, so a shortfall settled before the ranking
/// and one settled during it read the same.
fn bounded_section(
    section: Section<SemanticResultOutput>,
    unread_paths: Vec<String>,
) -> Section<SemanticResultOutput> {
    with_lower_bounds(
        section,
        &Vec::from_iter(
            (!unread_paths.is_empty()).then_some(LowerBound::ScanCouldNotReadPaths(unread_paths)),
        ),
    )
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
fn discover_files(app: &App) -> DiscoveredCorpus {
    let extensions: Vec<&str> = Language::all()
        .into_iter()
        .filter(|lang| is_code_language(*lang))
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();

    let filter = FileFilter::new(app.root());
    let discovery = filter.discover_files(&extensions);
    let mut files = discovery.files;
    files.sort();

    let mut corpus = DiscoveredCorpus {
        files: Vec::new(),
        unread_paths: discovery
            .unreadable
            .iter()
            .map(|path| UnreadPath {
                path: path.display().to_string(),
                is_file: false,
            })
            .collect(),
    };
    for path in files {
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) => {
                if crate::infra::hides_content(&e) {
                    corpus.unread_paths.push(UnreadPath {
                        path: path.display().to_string(),
                        is_file: true,
                    });
                }
                continue;
            }
        };
        if metadata.len() > PACK_MAX_FILE_BYTES {
            continue;
        }
        // Nanosecond resolution so two edits in the same wall-clock second
        // still register as a change and re-embed.
        let Some(mtime) = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
        else {
            corpus.unread_paths.push(UnreadPath {
                path: path.display().to_string(),
                is_file: true,
            });
            continue;
        };
        let rel = app.output.relative_path(&path);
        corpus.files.push(DiscoveredFile { path, rel, mtime });
    }
    corpus
}

/// The files semantic search will embed and rank, and how many paths it could
/// not read. A corpus short by an unreadable path ranks a smaller candidate
/// set than the project holds, which is what makes its count a lower bound.
struct DiscoveredCorpus {
    files: Vec<DiscoveredFile>,
    unread_paths: Vec<UnreadPath>,
}

/// Whether a file belongs to the requested language. No filter matches
/// everything, and a filter matches only its own files — never a silent "rank
/// everything". An unrecognized name never reaches here: the command refuses
/// it at the boundary, as every other `--lang` does, because a zero from a
/// typo reads as "no such code" rather than as "no such language".
fn language_matches(file: &DiscoveredFile, language: Option<Language>) -> bool {
    match language {
        None => true,
        Some(lang) => Language::from_path(&file.path) == lang,
    }
}

/// Re-embed every file of the requested `language` whose mtime moved, so the
/// cache holds that language complete and current. With no filter, every code
/// file is refreshed. Pruning is language-agnostic, so one language's vectors
/// never evict another's. Cost is incremental — only changed files run
/// through the model, and a `--lang` query never embeds another language.
fn refresh_cache(
    cache: &EmbeddingCache,
    provider: &dyn EmbeddingProvider,
    files: &[DiscoveredFile],
    language: Option<Language>,
) -> Result<Vec<String>> {
    let mut unread = Vec::new();
    for file in files {
        if !language_matches(file, language) {
            continue;
        }
        if cache.cached_mtime(&file.rel) == Some(file.mtime) {
            continue;
        }
        // The mtime moved, so whatever the cache holds for this file is known
        // to describe content that no longer exists. Failing to read the new
        // content leaves the old vectors in place, and they would rank as a
        // current match — a plausible answer about text the file no longer
        // has. Forgetting the file entirely keeps the ranking honest and
        // leaves nothing for a later run to mistake for a current embedding.
        let content = match std::fs::read_to_string(&file.path) {
            Ok(content) => content,
            Err(e) => {
                if crate::infra::hides_text(&e) {
                    unread.push(file.rel.clone());
                }
                cache.remove_file(&file.rel)?;
                continue;
            }
        };
        let chunks = embed_chunks(provider, &chunk_file(&content))?;
        let lang_id = Language::from_path(&file.path).lsp_id();
        cache.put_file(&file.rel, file.mtime, lang_id, &chunks)?;
    }
    Ok(unread)
}

/// Fallback when the cache can't open: embed the requested language and rank
/// it in memory, scoring each chunk as it's produced and dropping the vector,
/// so peak memory stays O(limit) like the cached path.
fn rank_in_memory<F: Fn(&[f32]) -> f32>(
    provider: &dyn EmbeddingProvider,
    files: &[DiscoveredFile],
    language: Option<Language>,
    limit: usize,
    score: F,
    unread: &mut Vec<String>,
) -> Result<(Vec<RankedChunk>, usize)> {
    let mut top = TopK::new(limit);
    for file in files {
        if !language_matches(file, language) {
            continue;
        }
        let content = match std::fs::read_to_string(&file.path) {
            Ok(content) => content,
            Err(e) => {
                if crate::infra::hides_text(&e) {
                    unread.push(file.rel.clone());
                }
                continue;
            }
        };
        for chunk in embed_chunks(provider, &chunk_file(&content))? {
            let score = score(&chunk.vector);
            top.offer(RankedChunk {
                file: file.rel.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                snippet: chunk.snippet,
                score,
            });
        }
    }
    Ok(top.finish())
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

    /// Every path the cache holds — the ranking filter is the walk's active
    /// set, so a test about the cache's own contents names all of them.
    fn cached_paths(cache: &EmbeddingCache) -> HashSet<String> {
        cache.cached_paths()
    }

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
        refresh_cache(&cache, &provider, &files, None).unwrap();
        assert_eq!(
            cache
                .rank_top(None, &cached_paths(&cache), 0, |_| 0.0)
                .unwrap()
                .1,
            2
        );
        let after_first = embedded.load(Ordering::SeqCst);
        assert_eq!(after_first, 2);

        // Same mtimes: nothing is re-embedded.
        refresh_cache(&cache, &provider, &files, None).unwrap();
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
        refresh_cache(&cache, &provider, &moved, None).unwrap();
        assert_eq!(embedded.load(Ordering::SeqCst), after_first + 1);
        assert_eq!(
            cache
                .rank_top(None, &cached_paths(&cache), 0, |_| 0.0)
                .unwrap()
                .1,
            2
        );
    }

    #[test]
    fn refresh_reembeds_a_changed_file_regardless_of_corpus_size() {
        // A populated cache must never block re-embedding a file whose mtime
        // moved, or semantic search serves stale vectors forever.
        let dir = tempfile::tempdir().unwrap();
        let embedded = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            embedded: Arc::clone(&embedded),
        };
        let cache =
            EmbeddingCache::open(dir.path(), provider.model_id(), provider.dimension()).unwrap();

        for i in 0..64 {
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
        refresh_cache(&cache, &provider, &[changed], None).unwrap();
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
        // Belt and braces for a name the boundary refuses: even reaching
        // here, `Unknown` must not match, so a filter can never widen.
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
