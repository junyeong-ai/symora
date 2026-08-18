//! On-disk cache for semantic-search embeddings.
//!
//! Each repo gets a `.symora/embeddings.db` SQLite file keyed by the
//! project-relative path. We store one row per ~30-line chunk: its line
//! span, a display snippet, and the embedding vector as a little-endian
//! `f32` BLOB. A `search semantic` run replays the cache for files whose
//! mtime is unchanged and re-embeds only what moved, so the dominant cost
//! — running every chunk through the model — is paid once and amortized.
//!
//! The cache is bound to a model: the whole file is reset when the active
//! model id, vector dimension, or schema version changes, because vectors
//! one model produced are meaningless to another. Embeddings are
//! rebuildable, so a reset is always safe and beats decoding stale vectors.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::path::Path;

use rusqlite::{Connection, params};

use crate::error::StoreError;

/// Bumped whenever the on-disk layout changes shape. A mismatch resets the
/// cache rather than risking a stale decode.
const SCHEMA_VERSION: i32 = 1;

/// One embedded chunk: where it sits in the file, a preview for output, and
/// the vector itself.
#[derive(Debug, Clone)]
pub struct CachedChunk {
    pub start_line: u32,
    pub end_line: u32,
    pub snippet: String,
    pub vector: Vec<f32>,
}

/// A scored chunk ready for output — its file location, snippet, and the
/// relevance score the caller assigned. Carries no vector: ranking drops it.
#[derive(Debug, Clone)]
pub struct RankedChunk {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub snippet: String,
    pub score: f32,
}

/// Bounded top-`limit` accumulator. Offering every candidate keeps only the
/// highest-scoring `limit` of them, so ranking a large corpus never holds
/// more than `limit` results in memory while still counting the full total.
pub struct TopK {
    limit: usize,
    heap: BinaryHeap<MinScored>,
    total: usize,
}

impl TopK {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::new(),
            total: 0,
        }
    }

    /// Count this candidate and keep it only if it ranks among the best seen.
    pub fn offer(&mut self, mut chunk: RankedChunk) {
        self.total += 1;
        if self.limit == 0 {
            return;
        }
        // cosine clamps degenerate cases to 0.0, so a score is normally
        // finite; a non-finite one is pinned to the lowest rank so it can
        // never outrank a real match and the heap's total order stays sane.
        if !chunk.score.is_finite() {
            chunk.score = f32::NEG_INFINITY;
        }
        if self.heap.len() < self.limit {
            self.heap.push(MinScored(chunk));
            return;
        }
        // The heap is full: replace its weakest entry only if this scores
        // higher, using the same `total_cmp` order the heap is built on so the
        // eviction can never disagree with the heap's shape. Copy the weakest
        // score out first so the borrow ends before the pop.
        let weakest = self.heap.peek().map(|m| m.0.score);
        if weakest.is_some_and(|w| chunk.score.total_cmp(&w) == Ordering::Greater) {
            self.heap.pop();
            self.heap.push(MinScored(chunk));
        }
    }

    /// The kept chunks, highest score first, and the total number offered.
    pub fn finish(self) -> (Vec<RankedChunk>, usize) {
        let total = self.total;
        let mut items: Vec<RankedChunk> = self.heap.into_iter().map(|m| m.0).collect();
        items.sort_by(|a, b| b.score.total_cmp(&a.score));
        (items, total)
    }
}

/// Min-heap ordering by score: the *lowest*-scoring entry compares greatest,
/// so [`BinaryHeap::peek`]/`pop` surface it for eviction once the heap fills.
struct MinScored(RankedChunk);

impl PartialEq for MinScored {
    fn eq(&self, other: &Self) -> bool {
        self.0.score == other.0.score
    }
}
impl Eq for MinScored {}
impl Ord for MinScored {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.score.total_cmp(&self.0.score)
    }
}
impl PartialOrd for MinScored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct EmbeddingCache {
    conn: Connection,
}

impl EmbeddingCache {
    /// Open the cache for `project_root`, bound to `(model_id, dimension)`.
    /// Any drift in schema, model, or dimension drops every row.
    pub fn open(project_root: &Path, model_id: &str, dimension: usize) -> Result<Self, StoreError> {
        let dir = project_root.join(".symora");
        std::fs::create_dir_all(&dir).map_err(StoreError::Io)?;
        let db_path = dir.join("embeddings.db");

        let conn = Connection::open(&db_path).map_err(|e| StoreError::Database(e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        let cache = Self { conn };
        cache.reset_if_stale(model_id, dimension)?;
        Ok(cache)
    }

    /// Drop everything when the cache no longer matches the active model,
    /// vector dimension, or on-disk schema, then record the current binding.
    fn reset_if_stale(&self, model_id: &str, dimension: usize) -> Result<(), StoreError> {
        let read = |key: &str| -> Option<String> {
            self.conn
                .query_row(
                    "SELECT value FROM embed_meta WHERE key = ?1",
                    params![key],
                    |r| r.get::<_, String>(0),
                )
                .ok()
        };

        let matches = read("schema_version").as_deref() == Some(&SCHEMA_VERSION.to_string())
            && read("model_id").as_deref() == Some(model_id)
            && read("dimension").as_deref() == Some(&dimension.to_string());
        if matches {
            return Ok(());
        }

        // Reset and rebind in one transaction so a concurrent open never
        // observes emptied chunks alongside the old model binding.
        let db = |e: rusqlite::Error| StoreError::Database(e.to_string());
        let tx = self.conn.unchecked_transaction().map_err(db)?;
        tx.execute_batch("DELETE FROM embed_chunks; DELETE FROM embed_files;")
            .map_err(db)?;
        for (key, value) in [
            ("schema_version", SCHEMA_VERSION.to_string()),
            ("model_id", model_id.to_string()),
            ("dimension", dimension.to_string()),
        ] {
            tx.execute(
                "INSERT OR REPLACE INTO embed_meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(())
    }

    /// The mtime cached for `rel_path`, if the file has been embedded.
    pub fn cached_mtime(&self, rel_path: &str) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT mtime FROM embed_files WHERE path = ?1",
                params![rel_path],
                |r| r.get::<_, i64>(0),
            )
            .ok()
    }

    /// Replace every chunk cached for `rel_path` with `chunks` at `mtime`,
    /// tagged with the file's `language`, in one transaction so a file is
    /// never half-embedded.
    pub fn put_file(
        &self,
        rel_path: &str,
        mtime: i64,
        language: &str,
        chunks: &[CachedChunk],
    ) -> Result<(), StoreError> {
        let db = |e: rusqlite::Error| StoreError::Database(e.to_string());
        let tx = self.conn.unchecked_transaction().map_err(db)?;
        tx.execute(
            "INSERT OR REPLACE INTO embed_files (path, mtime) VALUES (?1, ?2)",
            params![rel_path, mtime],
        )
        .map_err(db)?;
        tx.execute(
            "DELETE FROM embed_chunks WHERE path = ?1",
            params![rel_path],
        )
        .map_err(db)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO embed_chunks \
                     (path, chunk_index, language, start_line, end_line, snippet, vector) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(db)?;
            for (index, chunk) in chunks.iter().enumerate() {
                stmt.execute(params![
                    rel_path,
                    index as i64,
                    language,
                    chunk.start_line as i64,
                    chunk.end_line as i64,
                    chunk.snippet,
                    encode_vector(&chunk.vector),
                ])
                .map_err(db)?;
            }
        }
        tx.commit().map_err(db)?;
        Ok(())
    }

    /// Score every cached chunk for `language` (all languages when `None`)
    /// with `score`, returning the top `limit` by descending score and the
    /// total number scored. Vectors are streamed one row at a time and
    /// dropped after scoring, so peak memory is O(limit), never the corpus.
    /// Rank the cached chunks of `language` that belong to `active`.
    ///
    /// The active set is what THIS run found on disk, and it is the filter
    /// rather than `prune` because the two answer different questions. Prune
    /// deletes, so it may only run when the walk saw everything; ranking has
    /// to exclude a row whenever the walk did not, or a file that changed or
    /// vanished behind a path this run could not read would keep scoring as a
    /// current match. Filtering costs nothing the walk did not already pay
    /// and, unlike deleting, is safe when the walk was incomplete.
    pub fn rank_top<F: Fn(&[f32]) -> f32>(
        &self,
        language: Option<&str>,
        active: &HashSet<String>,
        limit: usize,
        score: F,
    ) -> Result<(Vec<RankedChunk>, usize), StoreError> {
        let db = |e: rusqlite::Error| StoreError::Database(e.to_string());
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, start_line, end_line, snippet, vector \
                 FROM embed_chunks WHERE (?1 IS NULL OR language = ?1) \
                 ORDER BY path, chunk_index",
            )
            .map_err(db)?;
        let mut rows = stmt.query(params![language]).map_err(db)?;
        let mut top = TopK::new(limit);
        while let Some(r) = rows.next().map_err(db)? {
            let path: String = r.get(0).map_err(db)?;
            if !active.contains(&path) {
                continue;
            }
            let vector = decode_vector(&r.get::<_, Vec<u8>>(4).map_err(db)?);
            let score = score(&vector);
            top.offer(RankedChunk {
                file: path,
                start_line: r.get::<_, i64>(1).map_err(db)? as u32,
                end_line: r.get::<_, i64>(2).map_err(db)? as u32,
                snippet: r.get(3).map_err(db)?,
                score,
            });
        }
        Ok(top.finish())
    }

    /// Drop files no longer present in `active`. Returns the count removed.
    /// Every path the cache holds a chunk for. Test-facing: production code
    /// filters by the walk's active set, never by the cache's own contents.
    #[cfg(test)]
    pub fn cached_paths(&self) -> HashSet<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT path FROM embed_chunks")
            .expect("cached_paths query");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("cached_paths rows");
        rows.filter_map(Result::ok).collect()
    }

    pub fn prune(&self, active: &HashSet<String>) -> Result<usize, StoreError> {
        let db = |e: rusqlite::Error| StoreError::Database(e.to_string());
        let known: Vec<String> = self
            .conn
            .prepare("SELECT path FROM embed_files")
            .and_then(|mut stmt| stmt.query_map([], |r| r.get::<_, String>(0))?.collect())
            .map_err(db)?;
        let stale: Vec<&String> = known.iter().filter(|p| !active.contains(*p)).collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction().map_err(db)?;
        for path in &stale {
            delete_file_rows(&tx, path).map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(stale.len())
    }

    /// Forget everything cached for `rel_path`.
    ///
    /// A file whose content could not be read is left with no row rather than
    /// a row recording the failure. The mtime is how the next run decides
    /// whether to embed, so any mtime written on a failure path is a value the
    /// file can come to hold again — a timestamp-preserving restore, or a
    /// permission change, which does not move mtime at all — and the run that
    /// meets it would read the record as "already current" and skip a file it
    /// has never embedded. Absence has no value to collide with.
    pub fn remove_file(&self, rel_path: &str) -> Result<(), StoreError> {
        let db = |e: rusqlite::Error| StoreError::Database(e.to_string());
        let tx = self.conn.unchecked_transaction().map_err(db)?;
        delete_file_rows(&tx, rel_path).map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(())
    }
}

fn delete_file_rows(conn: &rusqlite::Connection, rel_path: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM embed_chunks WHERE path = ?1",
        params![rel_path],
    )?;
    conn.execute("DELETE FROM embed_files WHERE path = ?1", params![rel_path])?;
    Ok(())
}

/// Pack an embedding into a little-endian `f32` byte blob.
fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Inverse of [`encode_vector`]; trailing bytes that don't form a full
/// `f32` are dropped (a corrupt row simply ranks as a zero vector).
fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS embed_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS embed_files (
    path  TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL
);

-- `language` is denormalized onto each chunk (it's file-level data) so the
-- ranking query filters and scans one indexed table with no join on the hot
-- path. `put_file` writes a file's rows in one transaction, so every chunk of
-- a file always carries the same language.
CREATE TABLE IF NOT EXISTS embed_chunks (
    path        TEXT NOT NULL,
    chunk_index INTEGER NOT NULL,
    language    TEXT NOT NULL,
    start_line  INTEGER NOT NULL,
    end_line    INTEGER NOT NULL,
    snippet     TEXT NOT NULL,
    vector      BLOB NOT NULL,
    PRIMARY KEY (path, chunk_index)
);

CREATE INDEX IF NOT EXISTS idx_embed_chunks_path ON embed_chunks(path);
CREATE INDEX IF NOT EXISTS idx_embed_chunks_language ON embed_chunks(language);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn chunk(start: u32, vector: Vec<f32>) -> CachedChunk {
        CachedChunk {
            start_line: start,
            end_line: start + 29,
            snippet: format!("chunk at {start}"),
            vector,
        }
    }

    /// Count cached chunks for `language` — `rank_top` with limit 0 keeps no
    /// items but still tallies the total. `active` names every cached path so
    /// the count is of the cache rather than of one walk's view of it.
    fn count(cache: &EmbeddingCache, language: Option<&str>) -> usize {
        cache
            .rank_top(language, &all_cached_paths(cache), 0, |_| 0.0)
            .unwrap()
            .1
    }

    fn all_cached_paths(cache: &EmbeddingCache) -> HashSet<String> {
        cache.cached_paths()
    }

    #[test]
    fn topk_keeps_highest_with_ties_and_is_non_finite_safe() {
        let ranked = |score: f32| RankedChunk {
            file: "f".into(),
            start_line: 1,
            end_line: 1,
            snippet: String::new(),
            score,
        };

        // Top 2 by score; a tie at the cutoff is kept by arrival order.
        let mut top = TopK::new(2);
        for s in [1.0, 5.0, 3.0, 5.0] {
            top.offer(ranked(s));
        }
        let (items, total) = top.finish();
        assert_eq!(total, 4);
        assert_eq!(
            items.iter().map(|r| r.score).collect::<Vec<_>>(),
            vec![5.0, 5.0]
        );

        // A non-finite score never displaces a real match.
        let mut top = TopK::new(1);
        top.offer(ranked(0.5));
        top.offer(ranked(f32::NAN));
        let (items, total) = top.finish();
        assert_eq!(total, 2);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].score, 0.5);
    }

    #[test]
    fn encode_decode_round_trips_a_vector() {
        let v = vec![0.5, -0.5, 1.0, 0.0, 3.25];
        assert_eq!(decode_vector(&encode_vector(&v)), v);
    }

    #[test]
    fn put_then_rank_returns_the_chunk() {
        let dir = TempDir::new().unwrap();
        let cache = EmbeddingCache::open(dir.path(), "model-a", 3).unwrap();
        cache
            .put_file("src/a.rs", 100, "rust", &[chunk(1, vec![0.5, -0.5, 1.0])])
            .unwrap();

        // The scorer reads the decoded vector, so this also proves the BLOB
        // round-tripped intact.
        let (items, total) = cache
            .rank_top(None, &all_cached_paths(&cache), 10, |v| v[2])
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].file, "src/a.rs");
        assert_eq!(items[0].snippet, "chunk at 1");
        assert_eq!(items[0].score, 1.0);
        assert_eq!(cache.cached_mtime("src/a.rs"), Some(100));
    }

    #[test]
    fn put_file_replaces_prior_chunks() {
        let dir = TempDir::new().unwrap();
        let cache = EmbeddingCache::open(dir.path(), "model-a", 2).unwrap();
        cache
            .put_file(
                "src/a.rs",
                1,
                "rust",
                &[chunk(1, vec![1.0, 0.0]), chunk(31, vec![0.0, 1.0])],
            )
            .unwrap();
        cache
            .put_file("src/a.rs", 2, "rust", &[chunk(1, vec![1.0, 1.0])])
            .unwrap();

        assert_eq!(count(&cache, None), 1);
        assert_eq!(cache.cached_mtime("src/a.rs"), Some(2));
    }

    #[test]
    fn changing_model_resets_the_cache() {
        let dir = TempDir::new().unwrap();
        {
            let cache = EmbeddingCache::open(dir.path(), "model-a", 2).unwrap();
            cache
                .put_file("src/a.rs", 1, "rust", &[chunk(1, vec![1.0, 0.0])])
                .unwrap();
            assert_eq!(count(&cache, None), 1);
        }
        // Reopening with a different model id must drop the stale vectors.
        let cache = EmbeddingCache::open(dir.path(), "model-b", 2).unwrap();
        assert_eq!(count(&cache, None), 0);
        assert_eq!(cache.cached_mtime("src/a.rs"), None);
    }

    #[test]
    fn changing_dimension_resets_the_cache() {
        let dir = TempDir::new().unwrap();
        {
            let cache = EmbeddingCache::open(dir.path(), "model-a", 2).unwrap();
            cache
                .put_file("src/a.rs", 1, "rust", &[chunk(1, vec![1.0, 0.0])])
                .unwrap();
        }
        let cache = EmbeddingCache::open(dir.path(), "model-a", 768).unwrap();
        assert_eq!(count(&cache, None), 0);
    }

    #[test]
    fn rank_top_filters_by_language_and_caps_items_not_total() {
        let dir = TempDir::new().unwrap();
        let cache = EmbeddingCache::open(dir.path(), "model-a", 1).unwrap();
        for i in 0..5 {
            cache
                .put_file(
                    &format!("src/f{i}.rs"),
                    1,
                    "rust",
                    &[chunk(1, vec![i as f32])],
                )
                .unwrap();
        }
        for i in 0..2 {
            cache
                .put_file(
                    &format!("src/g{i}.py"),
                    1,
                    "python",
                    &[chunk(1, vec![i as f32])],
                )
                .unwrap();
        }

        assert_eq!(count(&cache, None), 7);
        let (rust, rust_total) = cache
            .rank_top(Some("rust"), &all_cached_paths(&cache), 100, |_| 0.0)
            .unwrap();
        assert_eq!(rust_total, 5);
        assert!(rust.iter().all(|r| r.file.ends_with(".rs")));
        let (python, py_total) = cache
            .rank_top(Some("python"), &all_cached_paths(&cache), 100, |_| 0.0)
            .unwrap();
        assert_eq!(py_total, 2);
        assert!(python.iter().all(|r| r.file.ends_with(".py")));

        // The limit caps the items returned; the total stays the full count.
        let (top2, total) = cache
            .rank_top(None, &all_cached_paths(&cache), 2, |_| 0.0)
            .unwrap();
        assert_eq!(total, 7);
        assert_eq!(top2.len(), 2);
    }

    #[test]
    fn rank_top_returns_the_highest_scores_first() {
        let dir = TempDir::new().unwrap();
        let cache = EmbeddingCache::open(dir.path(), "model-a", 1).unwrap();
        for i in 0..5u32 {
            cache
                .put_file(
                    &format!("src/f{i}.rs"),
                    1,
                    "rust",
                    &[chunk(i, vec![i as f32])],
                )
                .unwrap();
        }
        // Score is the vector's single component; the top 3 must be the
        // largest three in descending order — proving the heap, not path
        // order, decides the result.
        let (items, total) = cache
            .rank_top(None, &all_cached_paths(&cache), 3, |v| v[0])
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(
            items.iter().map(|r| r.score).collect::<Vec<_>>(),
            vec![4.0, 3.0, 2.0]
        );
    }

    /// Ranking excludes a row whenever the walk that produced the active set
    /// did not see its file, and that is a different question from pruning:
    /// prune deletes, so it may only run when the walk saw everything, while a
    /// run that was turned away from part of the tree must still not rank a
    /// file it could not confirm. Filtering costs nothing the walk did not
    /// already pay and, unlike deleting, is safe when the walk was incomplete.
    #[test]
    fn ranking_excludes_a_file_this_run_did_not_see() {
        let dir = TempDir::new().unwrap();
        let cache = EmbeddingCache::open(dir.path(), "model-a", 1).unwrap();
        cache
            .put_file("src/seen.rs", 1, "rust", &[chunk(1, vec![1.0])])
            .unwrap();
        cache
            .put_file("src/hidden.rs", 1, "rust", &[chunk(1, vec![9.0])])
            .unwrap();

        let seen_only: HashSet<String> = ["src/seen.rs".to_string()].into_iter().collect();
        let (ranked, scored) = cache
            .rank_top(Some("rust"), &seen_only, 10, |v| v[0])
            .unwrap();
        assert_eq!(scored, 1, "only the file this run saw was scored");
        assert_eq!(
            ranked.iter().map(|r| r.file.as_str()).collect::<Vec<_>>(),
            vec!["src/seen.rs"],
            "the higher-scoring row belongs to a file the walk never confirmed"
        );

        let (ranked, _) = cache
            .rank_top(Some("rust"), &all_cached_paths(&cache), 10, |v| v[0])
            .unwrap();
        assert_eq!(
            ranked.len(),
            2,
            "and a whole walk ranks everything the cache holds"
        );
    }

    #[test]
    fn prune_drops_inactive_files() {
        let dir = TempDir::new().unwrap();
        let cache = EmbeddingCache::open(dir.path(), "model-a", 1).unwrap();
        cache
            .put_file("src/keep.rs", 1, "rust", &[chunk(1, vec![1.0])])
            .unwrap();
        cache
            .put_file("src/drop.rs", 1, "rust", &[chunk(1, vec![2.0])])
            .unwrap();

        let active: HashSet<String> = ["src/keep.rs".to_string()].into();
        assert_eq!(cache.prune(&active).unwrap(), 1);
        assert_eq!(cache.cached_mtime("src/drop.rs"), None);
        assert_eq!(cache.cached_mtime("src/keep.rs"), Some(1));
    }
}
