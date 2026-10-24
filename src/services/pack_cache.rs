//! On-disk cache for the pack engine.
//!
//! Each repo gets a `.symora/pack-cache.db` SQLite file keyed by the
//! project-relative path. We store mtime + the derived artefacts (imports,
//! signatures) per file — what the file states, never what a request asked
//! to see of it, or a later request would be answered in an earlier one's
//! shape. A `build_pack` run replays the cache when mtimes
//! match and re-extracts otherwise, so warm rebuilds skip the dominant
//! `read_to_string` + per-language scan.
//!
//! Schema is intentionally tiny — caches are rebuildable, so a read or
//! decode failure is treated as a miss and a schema-version mismatch
//! recreates the table, rather than maintaining migrations.

use std::path::Path;

use rusqlite::{Connection, params};

use crate::error::StoreError;
use crate::models::symbol::Language;
use crate::services::pack::PackedSymbol;

/// Bumped whenever the on-disk layout changes shape (new column, changed
/// JSON shape inside a TEXT column, etc.). Mismatched versions recreate the
/// table rather than risking stale-decode bugs at read time.
const SCHEMA_VERSION: i32 = 6;

#[derive(Debug, Clone)]
pub struct CachedEntry {
    pub mtime: i64,
    pub language: Language,
    pub imports: Vec<String>,
    pub signatures: Option<Vec<PackedSymbol>>,
}

pub struct PackCache {
    conn: Connection,
    extractor: String,
}

impl PackCache {
    pub fn open(project_root: &Path) -> Result<Self, StoreError> {
        let dir = crate::services::state_dir::create(project_root).map_err(StoreError::Io)?;
        let db_path = dir.join("pack-cache.db");

        let conn = Connection::open(&db_path).map_err(|e| StoreError::Database(e.to_string()))?;
        conn.execute_batch(META_SCHEMA)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        // Recreate the table when the on-disk layout no longer matches what
        // this binary expects. Pack caches are rebuildable, so dropping the
        // table is always safe — and a version bump has to be able to change
        // the table's SHAPE, which clearing its rows would not.
        let stored: i32 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM pack_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let extractor = format!(
            "{}:{}",
            crate::services::symbols::SymbolExtractor::extraction_digest(),
            crate::services::imports::ImportExtractor::extraction_digest(),
        );
        let recorded: Option<String> = conn
            .query_row(
                "SELECT value FROM pack_meta WHERE key = 'extractor'",
                [],
                |row| row.get(0),
            )
            .ok();
        if stored != SCHEMA_VERSION || recorded.as_deref() != Some(&extractor) {
            conn.execute_batch("DROP TABLE IF EXISTS pack_files")
                .map_err(|e| StoreError::Database(e.to_string()))?;
            conn.execute(
                "INSERT OR REPLACE INTO pack_meta (key, value) VALUES ('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(|e| StoreError::Database(e.to_string()))?;
            conn.execute(
                "INSERT OR REPLACE INTO pack_meta (key, value) VALUES ('extractor', ?1)",
                params![extractor],
            )
            .map_err(|error| StoreError::Database(error.to_string()))?;
        }
        conn.execute_batch(FILES_SCHEMA)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(Self { conn, extractor })
    }

    pub fn get(&self, rel_path: &str) -> Option<CachedEntry> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT mtime, language, imports, signatures \
                 FROM pack_files WHERE path = ?1 AND extractor = ?2",
            )
            .ok()?;
        stmt.query_row(params![rel_path, &self.extractor], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .ok()
        .and_then(|(mtime, language, imports, signatures)| {
            Some(CachedEntry {
                mtime,
                language: language.parse::<Language>().ok()?,
                imports: serde_json::from_str(&imports).ok()?,
                signatures: serde_json::from_str(&signatures).ok()?,
            })
        })
    }

    pub fn put(&self, rel_path: &str, entry: &CachedEntry) -> Result<(), StoreError> {
        let imports = serde_json::to_string(&entry.imports)
            .map_err(|e| StoreError::Database(e.to_string()))?;
        let signatures = serde_json::to_string(&entry.signatures)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        self.conn
            .execute(
                "INSERT OR REPLACE INTO pack_files \
                 (path, mtime, language, imports, signatures, extractor) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    rel_path,
                    entry.mtime,
                    entry.language.lsp_id(),
                    imports,
                    signatures,
                    &self.extractor,
                ],
            )
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Drop entries whose path is not in `active`. Returns the number of
    /// rows deleted, mostly for observability.
    pub fn prune(&self, active: &std::collections::HashSet<String>) -> Result<usize, StoreError> {
        let known: Vec<String> = self
            .conn
            .prepare("SELECT path FROM pack_files WHERE extractor = ?1")
            .and_then(|mut stmt| {
                let rows: Result<Vec<String>, _> = stmt
                    .query_map(params![&self.extractor], |r| r.get::<_, String>(0))?
                    .collect();
                rows
            })
            .map_err(|e| StoreError::Database(e.to_string()))?;

        let stale: Vec<&String> = known.iter().filter(|p| !active.contains(*p)).collect();
        if stale.is_empty() {
            return Ok(0);
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| StoreError::Database(e.to_string()))?;
        for path in &stale {
            tx.execute(
                "DELETE FROM pack_files WHERE path = ?1 AND extractor = ?2",
                params![path, &self.extractor],
            )
            .map_err(|e| StoreError::Database(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(stale.len())
    }
}

const META_SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS pack_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const FILES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS pack_files (
    path TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL,
    language TEXT NOT NULL,
    imports TEXT NOT NULL,
    signatures TEXT NOT NULL,
    extractor TEXT NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(mtime: i64) -> CachedEntry {
        CachedEntry {
            mtime,
            language: Language::Rust,
            imports: vec!["crate::baz".into()],
            signatures: Some(vec![PackedSymbol {
                name: "foo".into(),
                kind: "function".into(),
                line: 12,
                signature: "pub fn foo()".into(),
            }]),
        }
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let cache = PackCache::open(dir.path()).unwrap();
        cache.put("src/foo.rs", &entry(100)).unwrap();
        let got = cache.get("src/foo.rs").unwrap();
        assert_eq!(got.mtime, 100);
        let signatures = got.signatures.unwrap();
        assert_eq!(signatures.len(), 1);
        assert_eq!(signatures[0].name, "foo");
    }

    #[test]
    fn put_overwrites_existing_entry() {
        let dir = TempDir::new().unwrap();
        let cache = PackCache::open(dir.path()).unwrap();
        cache.put("src/foo.rs", &entry(100)).unwrap();
        cache.put("src/foo.rs", &entry(200)).unwrap();
        assert_eq!(cache.get("src/foo.rs").unwrap().mtime, 200);
    }

    #[test]
    fn prune_drops_inactive_paths() {
        let dir = TempDir::new().unwrap();
        let cache = PackCache::open(dir.path()).unwrap();
        cache.put("src/keep.rs", &entry(1)).unwrap();
        cache.put("src/drop.rs", &entry(2)).unwrap();

        let active: std::collections::HashSet<String> = ["src/keep.rs".to_string()].into();
        let dropped = cache.prune(&active).unwrap();
        assert_eq!(dropped, 1);
        assert!(cache.get("src/keep.rs").is_some());
        assert!(cache.get("src/drop.rs").is_none());
    }

    #[test]
    fn entries_require_the_current_import_extractor() {
        let dir = TempDir::new().unwrap();
        let cache = PackCache::open(dir.path()).unwrap();
        cache.put("src/foo.rs", &entry(100)).unwrap();
        cache
            .conn
            .execute(
                "UPDATE pack_meta SET value = ?1 WHERE key = 'extractor'",
                params![crate::services::symbols::SymbolExtractor::extraction_digest().to_string()],
            )
            .unwrap();
        drop(cache);
        let cache = PackCache::open(dir.path()).unwrap();
        assert!(cache.get("src/foo.rs").is_none());
        cache.put("src/foo.rs", &entry(100)).unwrap();
        drop(cache);
        assert!(
            PackCache::open(dir.path())
                .unwrap()
                .get("src/foo.rs")
                .is_some()
        );
    }

    #[test]
    fn missing_entry_returns_none() {
        let dir = TempDir::new().unwrap();
        let cache = PackCache::open(dir.path()).unwrap();
        assert!(cache.get("does/not/exist.rs").is_none());
    }
    #[test]
    fn simultaneous_producers_cannot_replay_each_others_entries() {
        let root = TempDir::new().unwrap();
        let mut earlier = PackCache::open(root.path()).unwrap();
        earlier.extractor = "earlier-producer".to_string();
        let current = PackCache::open(root.path()).unwrap();
        earlier.put("a.rs", &entry(1)).unwrap();
        assert!(current.get("a.rs").is_none());
        assert_eq!(current.prune(&std::collections::HashSet::new()).unwrap(), 0);
        assert!(earlier.get("a.rs").is_some());
        current.put("a.rs", &entry(2)).unwrap();
        assert!(earlier.get("a.rs").is_none());
        assert_eq!(current.get("a.rs").unwrap().mtime, 2);
    }
}
