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

use std::collections::HashSet;
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

    /// Every cached chunk for `language` (all languages when `None`), paired
    /// with its file path. The full set is returned so the caller scores all
    /// of it — the best semantic match is never hidden behind a row cap.
    pub fn load_corpus(
        &self,
        language: Option<&str>,
    ) -> Result<Vec<(String, CachedChunk)>, StoreError> {
        let db = |e: rusqlite::Error| StoreError::Database(e.to_string());
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, start_line, end_line, snippet, vector \
                 FROM embed_chunks WHERE (?1 IS NULL OR language = ?1) \
                 ORDER BY path, chunk_index",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map(params![language], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    CachedChunk {
                        start_line: r.get::<_, i64>(1)? as u32,
                        end_line: r.get::<_, i64>(2)? as u32,
                        snippet: r.get::<_, String>(3)?,
                        vector: decode_vector(&r.get::<_, Vec<u8>>(4)?),
                    },
                ))
            })
            .map_err(db)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Drop files no longer present in `active`. Returns the count removed.
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
            tx.execute("DELETE FROM embed_chunks WHERE path = ?1", params![path])
                .map_err(db)?;
            tx.execute("DELETE FROM embed_files WHERE path = ?1", params![path])
                .map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(stale.len())
    }
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

    #[test]
    fn put_then_load_round_trips_vectors() {
        let dir = TempDir::new().unwrap();
        let cache = EmbeddingCache::open(dir.path(), "model-a", 3).unwrap();
        cache
            .put_file("src/a.rs", 100, "rust", &[chunk(1, vec![0.5, -0.5, 1.0])])
            .unwrap();

        let corpus = cache.load_corpus(None).unwrap();
        assert_eq!(corpus.len(), 1);
        assert_eq!(corpus[0].0, "src/a.rs");
        assert_eq!(corpus[0].1.vector, vec![0.5, -0.5, 1.0]);
        assert_eq!(corpus[0].1.snippet, "chunk at 1");
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

        assert_eq!(cache.load_corpus(None).unwrap().len(), 1);
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
            assert_eq!(cache.load_corpus(None).unwrap().len(), 1);
        }
        // Reopening with a different model id must drop the stale vectors.
        let cache = EmbeddingCache::open(dir.path(), "model-b", 2).unwrap();
        assert_eq!(cache.load_corpus(None).unwrap().len(), 0);
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
        assert_eq!(cache.load_corpus(None).unwrap().len(), 0);
    }

    #[test]
    fn load_corpus_filters_by_language_returning_the_full_set() {
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

        // No filter returns the whole corpus; a language filter returns that
        // language complete — never a capped prefix.
        assert_eq!(cache.load_corpus(None).unwrap().len(), 7);
        let rust = cache.load_corpus(Some("rust")).unwrap();
        assert_eq!(rust.len(), 5);
        assert!(rust.iter().all(|(path, _)| path.ends_with(".rs")));
        let python = cache.load_corpus(Some("python")).unwrap();
        assert_eq!(python.len(), 2);
        assert!(python.iter().all(|(path, _)| path.ends_with(".py")));
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
