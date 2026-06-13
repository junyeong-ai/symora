use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension;

use super::db::SqliteDb;
use super::schema::*;
use super::symbols::SymbolExtractor;
use super::types::*;
use crate::error::StoreError;
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::{Language, SymbolKind};

/// Extensions a full index pass covers when no language filter narrows it.
/// Also the domain an unrestricted build's `refresh_files` honors, so an
/// edit can never index a file kind a build wouldn't.
const INDEXED_EXTENSIONS: &[&str] = &[
    "rs", "go", "py", "ts", "tsx", "js", "jsx", "java", "kt", "kts", "cpp", "cc", "cxx", "c", "h",
    "hpp", "cs", "rb", "php", "lua", "sh",
];

/// Meta key recording that a full build completed, and what it covered.
/// Its presence IS the build-completed marker: a store without it was
/// never built (merely opening the DB for a read materializes the file,
/// so file existence proves nothing), and `refresh_files` must not grow
/// a 1-file index inside it.
const META_BUILD_SCOPE: &str = "build_scope";

/// The language scope the last completed build covered. Persisted in the
/// store's `meta` table so per-file refreshes honor the build's narrowing
/// (`search index build --lang rust` must not gain `.py` rows from an
/// edit) and so a never-built store is recognizable regardless of whether
/// the DB file exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildScope {
    /// Unrestricted build: every extension in `INDEXED_EXTENSIONS`.
    All,
    Languages(Vec<Language>),
}

impl BuildScope {
    fn from_options(languages: &Option<Vec<Language>>) -> Self {
        match languages {
            None => Self::All,
            Some(langs) => {
                let mut langs = langs.clone();
                langs.sort_by_key(|l| l.lsp_id());
                langs.dedup();
                Self::Languages(langs)
            }
        }
    }

    /// The extension set a build under this scope discovers — also the
    /// domain a refresh honors.
    fn extensions(&self) -> Vec<&'static str> {
        match self {
            Self::All => INDEXED_EXTENSIONS.to_vec(),
            Self::Languages(langs) => langs.iter().flat_map(|l| l.extensions()).copied().collect(),
        }
    }

    fn meta_value(&self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Languages(langs) => langs
                .iter()
                .map(|l| l.lsp_id())
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    fn parse(value: &str) -> Self {
        if value == "all" {
            return Self::All;
        }
        Self::Languages(
            value
                .split(',')
                .filter(|s| !s.is_empty())
                .map(Language::parse_or_default)
                .collect(),
        )
    }
}

pub struct Store {
    db: SqliteDb,
    project_root: PathBuf,
    config: StoreConfig,
    symbol_extractor: SymbolExtractor,
    is_indexing: AtomicBool,
    index_ready: AtomicBool,
}

impl Store {
    /// On-disk location of the index for a project — the single source of
    /// truth for the path, shared by `open` and callers that only need to
    /// know whether an index exists.
    pub fn db_path(project_root: &Path) -> PathBuf {
        project_root.join(".symora").join("store.db")
    }

    pub async fn open(project_root: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        let db_path = Self::db_path(project_root);
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(StoreError::Io)?;
        }

        let db = match Self::try_open_db(&db_path).await {
            Ok(db) => db,
            // A clean schema upgrade is an expected migration, not corruption:
            // the rebuild action is identical, but classify it honestly so the
            // log doesn't cry "corrupted" on every version bump.
            Err(StoreError::SchemaMismatch { found, expected }) => {
                tracing::info!(
                    "Store schema changed (db v{found} -> v{expected}), rebuilding index: {}",
                    db_path.display()
                );
                Self::recover_db(&db_path).await?
            }
            Err(e) => {
                tracing::warn!(
                    "Store database unreadable, recreating: {}: {e}",
                    db_path.display()
                );
                Self::recover_db(&db_path).await?
            }
        };

        let has_data: bool = db
            .call(|conn| {
                Ok(conn
                    .query_row::<i64, _, _>("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                    .unwrap_or(0)
                    > 0)
            })
            .await?;

        Ok(Self {
            db,
            project_root: project_root.to_path_buf(),
            config,
            symbol_extractor: SymbolExtractor::new(),
            is_indexing: AtomicBool::new(false),
            index_ready: AtomicBool::new(has_data),
        })
    }

    async fn try_open_db(db_path: &Path) -> Result<SqliteDb, StoreError> {
        let db = SqliteDb::open(db_path).await?;

        let version: i32 = db
            .call(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0)))
            .await?;

        if version != 0 && version != SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch {
                found: version,
                expected: SCHEMA_VERSION,
            });
        }

        db.execute(INIT_SCHEMA).await?;
        db.call(|conn| {
            conn.query_row("SELECT 1", [], |_r| Ok(()))?;
            Ok(())
        })
        .await?;
        Ok(db)
    }

    async fn recover_db(db_path: &Path) -> Result<SqliteDb, StoreError> {
        if db_path.exists() {
            let backup_path = db_path.with_extension("db.bak");
            if let Err(e) = tokio::fs::rename(db_path, &backup_path).await {
                tracing::debug!("Failed to backup corrupt DB: {e}");
            }
        }
        let db = SqliteDb::open(db_path).await?;
        db.execute(INIT_SCHEMA).await?;
        Ok(db)
    }

    pub fn is_indexing(&self) -> bool {
        self.is_indexing.load(Ordering::SeqCst)
    }

    pub async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind_filter: Option<SymbolKind>,
    ) -> Result<SearchPage<SymbolSearchResult>, StoreError> {
        if !self.index_ready.load(Ordering::SeqCst) {
            return Err(StoreError::NotInitialized);
        }

        let query = query.to_string();
        let limit = limit as i64;
        let kind_str = kind_filter.map(|k| k.to_string());

        let (mut page, snapshot) = self
            .db
            .call(move |conn| {
                let sql = build_symbol_search_query(kind_str.is_some());
                let mut stmt = conn.prepare(&sql)?;
                let rows = match &kind_str {
                    Some(k) => stmt.query(rusqlite::params![query, limit, k])?,
                    None => stmt.query(rusqlite::params![query, limit])?,
                };

                let mut total = 0usize;
                let rows: Vec<SymbolSearchResult> = rows
                    .mapped(|r| {
                        Ok((
                            r.get::<_, i64>(7)? as usize,
                            SymbolSearchResult {
                                name: r.get(0)?,
                                name_path: r.get(1)?,
                                kind: SymbolKind::parse_or_default(&r.get::<_, String>(2)?),
                                line: r.get::<_, i32>(3)? as u32,
                                column: r.get::<_, i32>(4)? as u32,
                                file: PathBuf::from(r.get::<_, String>(5)?),
                                container: r.get(6)?,
                                score: r.get(8)?,
                            },
                        ))
                    })
                    .filter_map(|r| match r {
                        Ok((row_total, v)) => {
                            total = row_total;
                            Some(v)
                        }
                        Err(e) => {
                            tracing::debug!("Error parsing symbol search row: {}", e);
                            None
                        }
                    })
                    .collect();

                // Snapshot the indexed hash of each backing file in the SAME
                // connection closure as the row query, so a concurrent
                // reindex cannot land between reading the rows and reading
                // the hashes they were derived from.
                let snapshot = indexed_hashes(conn, rows.iter().map(|r| &r.file))?;
                let page = SearchPage {
                    total,
                    rows,
                    stale: false,
                };
                Ok((page, snapshot))
            })
            .await?;

        page.stale = any_backing_file_changed(snapshot).await;
        Ok(page)
    }

    pub async fn search_content(
        &self,
        query: &str,
        limit: usize,
        language: Option<Language>,
    ) -> Result<SearchPage<ContentSearchResult>, StoreError> {
        if !self.index_ready.load(Ordering::SeqCst) {
            return Err(StoreError::NotInitialized);
        }

        let query = query.to_string();
        let limit = limit as i64;
        let lang_str = language.map(|l| l.lsp_id().to_string());
        // The trigram pre-filter needs >= 3 chars; shorter queries fall back to
        // the LIKE-only scan (the deterministic threshold, not a guess).
        let use_fts = query.chars().count() >= FTS_MIN_QUERY_CHARS;

        let (mut page, snapshot) = self
            .db
            .call(move |conn| {
                let sql = build_content_search_query(lang_str.is_some(), use_fts);
                let mut stmt = conn.prepare(&sql)?;
                let rows = match &lang_str {
                    Some(l) => stmt.query(rusqlite::params![query, limit, l])?,
                    None => stmt.query(rusqlite::params![query, limit])?,
                };

                let mut total = 0usize;
                let rows: Vec<ContentSearchResult> = rows
                    .mapped(|r| {
                        Ok((
                            r.get::<_, i64>(4)? as usize,
                            ContentSearchResult {
                                content: r.get(0)?,
                                line: r.get::<_, i32>(1)? as u32,
                                file: PathBuf::from(r.get::<_, String>(2)?),
                                score: r.get(5)?,
                            },
                        ))
                    })
                    .filter_map(|r| match r {
                        Ok((row_total, v)) => {
                            total = row_total;
                            Some(v)
                        }
                        Err(e) => {
                            tracing::debug!("Error parsing content search row: {}", e);
                            None
                        }
                    })
                    .collect();

                let snapshot = indexed_hashes(conn, rows.iter().map(|r| &r.file))?;
                let page = SearchPage {
                    total,
                    rows,
                    stale: false,
                };
                Ok((page, snapshot))
            })
            .await?;

        page.stale = any_backing_file_changed(snapshot).await;
        Ok(page)
    }

    /// Bring just-edited files' rows in line with the bytes on disk:
    /// re-extract while a file exists and the last build's scope covers
    /// it, drop its rows when it doesn't. This is the edit flow's
    /// endpoint — a write becomes searchable immediately instead of
    /// leaving a hole until the next `index build`.
    ///
    /// A store with no completed build is left untouched: opening the DB
    /// for a read already materializes the file, so the build marker —
    /// not file existence — decides "never built", and an edit must not
    /// grow a 1-file index that would then answer authoritatively.
    ///
    /// Failures keep the old rows (the next read sees them flagged
    /// `stale`) and propagate to the caller, which logs the disclosed
    /// warn — the edit itself already succeeded and stays successful.
    pub async fn refresh_files(&self, paths: &[PathBuf]) -> Result<(), StoreError> {
        let Some(scope) = self.build_scope().await? else {
            tracing::debug!("Skipping index refresh: the index was never built");
            return Ok(());
        };
        // One ignore-rules build for the whole batch — multi-file
        // operations (rename, actions apply) refresh many files at once.
        let filter = FileFilter::with_gitignore(&self.project_root);
        let extensions = scope.extensions();

        let mut first_err: Option<StoreError> = None;
        for path in paths {
            let result = if Self::is_indexable(path, &extensions, &filter)
                && tokio::fs::try_exists(path).await.unwrap_or(false)
            {
                self.index_file(path).await
            } else {
                self.remove_file_rows(path).await
            };
            if let Err(e) = result {
                tracing::debug!("Failed to refresh {} in index: {e}", path.display());
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Whether the last build would cover this file: an extension inside
    /// the recorded build scope, not excluded by the project's ignore
    /// rules.
    fn is_indexable(path: &Path, scope_extensions: &[&str], filter: &FileFilter) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| scope_extensions.contains(&ext))
            && filter.should_include(path)
    }

    /// The scope of the last completed build, or `None` when no build has
    /// ever completed against this store.
    async fn build_scope(&self) -> Result<Option<BuildScope>, StoreError> {
        let value: Option<String> = self
            .db
            .call(|conn| {
                conn.query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    rusqlite::params![META_BUILD_SCOPE],
                    |r| r.get(0),
                )
                .optional()
            })
            .await?;
        Ok(value.as_deref().map(BuildScope::parse))
    }

    async fn record_build_scope(&self, scope: &BuildScope) -> Result<(), StoreError> {
        let value = scope.meta_value();
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![META_BUILD_SCOPE, value],
                )?;
                Ok(())
            })
            .await
    }

    async fn remove_file_rows(&self, path: &Path) -> Result<(), StoreError> {
        let path_str = path.display().to_string();
        self.db
            .call(move |conn| {
                let file_id: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM files WHERE path = ?1",
                        rusqlite::params![&path_str],
                        |r| r.get(0),
                    )
                    .ok();

                if let Some(fid) = file_id {
                    delete_file_and_data(conn, fid)?;
                }
                Ok(())
            })
            .await
    }

    pub async fn cleanup_expired(&self) -> usize {
        let cutoff = (now_unix() - self.config.ttl_secs) as i64;
        self.db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let count = tx.query_row(
                    "SELECT COUNT(*) FROM files WHERE indexed_at < ?1",
                    rusqlite::params![cutoff],
                    |r| r.get::<_, i64>(0),
                )? as usize;

                if count > 0 {
                    tx.execute(
                        "DELETE FROM symbols WHERE file_id IN (SELECT id FROM files WHERE indexed_at < ?1)",
                        rusqlite::params![cutoff],
                    )?;
                    tx.execute(
                        "DELETE FROM content_lines WHERE file_id IN (SELECT id FROM files WHERE indexed_at < ?1)",
                        rusqlite::params![cutoff],
                    )?;
                    tx.execute("DELETE FROM files WHERE indexed_at < ?1", rusqlite::params![cutoff])?;
                }
                tx.commit()?;
                Ok(count)
            })
            .await
            .unwrap_or(0)
    }

    /// Empty the index, including the build-completed marker: a cleared
    /// store is "never built" again, so a stray per-file refresh can't
    /// start regrowing a partial index inside it.
    pub async fn clear(&self) -> Result<(), StoreError> {
        self.db
            .execute(
                "DELETE FROM content_lines; DELETE FROM symbols; DELETE FROM files; \
                 DELETE FROM meta; VACUUM;",
            )
            .await?;
        self.index_ready.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub async fn checkpoint(&self) -> Result<(), StoreError> {
        self.db.execute("PRAGMA wal_checkpoint(TRUNCATE);").await
    }

    pub async fn stats(&self) -> Result<IndexStats, StoreError> {
        let db_path = Self::db_path(&self.project_root);
        let index_size_bytes = tokio::fs::metadata(&db_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let is_indexing = self.is_indexing.load(Ordering::SeqCst);

        self.db
            .call(move |conn| {
                let count_of = |table: &str| {
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap_or(0) as usize
                };
                Ok(IndexStats {
                    file_count: count_of("files"),
                    symbol_count: count_of("symbols"),
                    content_line_count: count_of("content_lines"),
                    index_size_bytes,
                    last_indexed: conn
                        .query_row("SELECT COALESCE(MAX(indexed_at), 0) FROM files", [], |r| {
                            r.get::<_, i64>(0)
                        })
                        .unwrap_or(0) as u64,
                    is_indexing,
                    progress: None,
                })
            })
            .await
    }

    pub async fn index(&self, options: IndexOptions) -> Result<IndexStats, StoreError> {
        if self.is_indexing.swap(true, Ordering::SeqCst) {
            return Err(StoreError::AlreadyIndexing);
        }

        let result = self.do_index(options).await;
        self.is_indexing.store(false, Ordering::SeqCst);
        result
    }

    async fn do_index(&self, options: IndexOptions) -> Result<IndexStats, StoreError> {
        let filter = FileFilter::with_gitignore(&self.project_root);
        let scope = BuildScope::from_options(&options.languages);
        let extensions = scope.extensions();

        let files = filter.discover_files(&extensions);
        let discovered_paths: std::collections::HashSet<String> = files
            .iter()
            .map(|path| path.display().to_string())
            .collect();

        if options.force {
            self.clear().await?;
        } else {
            self.prune_deleted_files(&discovered_paths).await?;
        }

        // Each future acquires its own permit when polled, so the semaphore
        // gates the fan-out: futures are created up-front, permits taken and
        // released as `join_all` drives them. Acquiring before the future is
        // built would instead reserve every permit eagerly and stall once the
        // file count crossed the concurrency cap.
        let concurrency = self.config.index_concurrency.max(1);
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
        let tasks: Vec<_> = files
            .iter()
            .map(|file| {
                let sem = std::sync::Arc::clone(&semaphore);
                async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    self.index_file(file).await
                }
            })
            .collect();
        for result in futures::future::join_all(tasks).await {
            if let Err(e) = result {
                tracing::warn!("Failed to index file: {}", e);
            }
        }

        // The durable build-completed marker, with the scope this build
        // covered. Last build wins on purpose: the pruning above already
        // narrows the row domain to this scope, so the recorded scope must
        // follow it — and per-file refreshes honor exactly this domain.
        self.record_build_scope(&scope).await?;
        self.index_ready.store(true, Ordering::SeqCst);
        let _ = self.checkpoint().await;
        self.stats().await
    }

    async fn index_file(&self, path: &Path) -> Result<(), StoreError> {
        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("Skipping file {}: {}", path.display(), e);
                return Ok(());
            }
        };

        let content_hash = crate::infra::hash_content(&content) as i64;

        let language = Language::from_path(path);
        let lang_str = match language {
            Language::Unknown => None,
            l => Some(l.lsp_id().to_string()),
        };
        let file_path = path.display().to_string();

        // Skip files whose content and language already match the index,
        // before paying for extraction or a write.
        if self.is_current(&file_path, content_hash, &lang_str).await? {
            return Ok(());
        }

        let symbols = self.symbol_extractor.extract(&content, language);
        let content_lines: Vec<(i32, String)> = if self.config.index_content {
            content
                .lines()
                .enumerate()
                .filter(|(_, line)| !line.trim().is_empty())
                .map(|(i, line)| ((i + 1) as i32, line.to_string()))
                .collect()
        } else {
            Vec::new()
        };
        let now = now_unix() as i64;

        // The file row and every row derived from it are written in one
        // transaction. A failed insert rolls back the content-hash stamp, so
        // a file is never recorded as current with missing symbols — it is
        // simply re-indexed on the next pass instead of silently disappearing.
        self.db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let file_id: i64 = tx.query_row(
                    "INSERT INTO files (path, content_hash, language, indexed_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(path) DO UPDATE SET
                         content_hash = excluded.content_hash,
                         language = excluded.language,
                         indexed_at = excluded.indexed_at
                     RETURNING id",
                    rusqlite::params![file_path, content_hash, lang_str, now],
                    |r| r.get(0),
                )?;
                delete_file_related_data(&tx, file_id)?;
                {
                    // A duplicate (line, col) is a benign extractor overlap
                    // (container and leaf at the same position); drop the
                    // extra rather than aborting the whole file.
                    let mut stmt = tx.prepare(
                        "INSERT OR IGNORE INTO symbols (file_id, name, name_path, kind, container, line, col) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    )?;
                    for s in &symbols {
                        stmt.execute(rusqlite::params![
                            file_id,
                            s.name,
                            s.name_path,
                            s.kind.to_string(),
                            s.container,
                            s.line as i32,
                            s.column as i32
                        ])?;
                    }
                }
                if !content_lines.is_empty() {
                    let mut stmt = tx.prepare(
                        "INSERT INTO content_lines (file_id, line_num, content) VALUES (?1, ?2, ?3)",
                    )?;
                    for (line_num, line) in &content_lines {
                        stmt.execute(rusqlite::params![file_id, line_num, line])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    /// True when the file's content hash and language already match the
    /// stored row, so identical content is never re-extracted and any change
    /// to the bytes is always re-indexed. The content hash is the currency
    /// key, which keeps even same-size, rapid edits from slipping through.
    async fn is_current(
        &self,
        file_path: &str,
        content_hash: i64,
        lang_str: &Option<String>,
    ) -> Result<bool, StoreError> {
        let file_path = file_path.to_string();
        let lang_str = lang_str.clone();
        self.db
            .call(move |conn| {
                let existing = conn
                    .query_row(
                        "SELECT content_hash, language FROM files WHERE path = ?1",
                        rusqlite::params![file_path],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
                    )
                    .optional()?;
                Ok(match existing {
                    Some((existing_hash, existing_lang)) => {
                        existing_hash == content_hash && existing_lang == lang_str
                    }
                    None => false,
                })
            })
            .await
    }

    async fn prune_deleted_files(
        &self,
        discovered_paths: &std::collections::HashSet<String>,
    ) -> Result<(), StoreError> {
        let known_files: Vec<(i64, String)> = self
            .db
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT id, path FROM files")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
                Ok(rows.filter_map(Result::ok).collect::<Vec<_>>())
            })
            .await?;

        let stale: Vec<i64> = known_files
            .into_iter()
            .filter(|(_, path)| !discovered_paths.contains(path) || !Path::new(path).exists())
            .map(|(id, _)| id)
            .collect();

        if stale.is_empty() {
            return Ok(());
        }

        self.db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                for id in stale {
                    delete_file_and_data(&tx, id)?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;

        Ok(())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Snapshot the indexed `content_hash` of each distinct file backing a
/// search page, read in the same connection closure as the rows themselves
/// so the pair is atomic against a concurrent reindex. A file absent from
/// the index maps to `None`.
fn indexed_hashes<'a>(
    conn: &rusqlite::Connection,
    files: impl Iterator<Item = &'a PathBuf>,
) -> Result<Vec<(String, Option<i64>)>, rusqlite::Error> {
    let mut distinct: Vec<String> = files.map(|p| p.display().to_string()).collect();
    distinct.sort_unstable();
    distinct.dedup();

    let mut stmt = conn.prepare("SELECT content_hash FROM files WHERE path = ?1")?;
    distinct
        .into_iter()
        .map(|path| {
            let stored: Option<i64> = stmt
                .query_row(rusqlite::params![path], |r| r.get(0))
                .optional()?;
            Ok((path, stored))
        })
        .collect()
}

/// True when any file in the snapshot no longer matches the indexed hash it
/// was served under — rewritten, deleted, or unreadable since `index()` ran.
/// Biased toward false positives on purpose: a spurious stale banner is
/// harmless, stale rows presented as current are not. Cost is one disk read
/// per distinct backing file, bounded by the page size.
async fn any_backing_file_changed(snapshot: Vec<(String, Option<i64>)>) -> bool {
    for (path, indexed_hash) in snapshot {
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                if indexed_hash != Some(crate::infra::hash_content(&content) as i64) {
                    return true;
                }
            }
            // Deleted or unreadable: the row is no longer backed by disk.
            Err(_) => return true,
        }
    }
    false
}

fn delete_file_related_data(
    conn: &rusqlite::Connection,
    file_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM symbols WHERE file_id = ?1",
        rusqlite::params![file_id],
    )?;
    conn.execute(
        "DELETE FROM content_lines WHERE file_id = ?1",
        rusqlite::params![file_id],
    )?;
    Ok(())
}

fn delete_file_and_data(conn: &rusqlite::Connection, file_id: i64) -> Result<(), rusqlite::Error> {
    delete_file_related_data(conn, file_id)?;
    conn.execute(
        "DELETE FROM files WHERE id = ?1",
        rusqlite::params![file_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn total_matches(store: &Store, query: &str) -> usize {
        store.search_symbols(query, 50, None).await.unwrap().total
    }

    /// Run the content-search SQL directly with a chosen `use_fts`, returning
    /// comparable (content, line, path, score) rows — so a test can assert the
    /// FTS pre-filter is set-identical to the LIKE-only scan.
    async fn content_rows(store: &Store, query: &str, use_fts: bool) -> Vec<(String, i64, f64)> {
        let q = query.to_string();
        store
            .db
            .call(move |conn| {
                let sql = build_content_search_query(false, use_fts);
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(rusqlite::params![q, 10_000_i64], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, f64>(5)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }

    async fn fts_row_count(store: &Store) -> i64 {
        store
            .db
            .call(|conn| conn.query_row("SELECT count(*) FROM content_lines_fts", [], |r| r.get(0)))
            .await
            .unwrap()
    }

    async fn content_row_count(store: &Store) -> i64 {
        store
            .db
            .call(|conn| conn.query_row("SELECT count(*) FROM content_lines", [], |r| r.get(0)))
            .await
            .unwrap()
    }

    /// The FTS trigram pre-filter must return exactly the same rows, scores, and
    /// order as the LIKE-only scan for every >= 3-char query, including
    /// FTS-syntax characters, mixed case, and non-ASCII text. Any divergence
    /// means the index path silently disagrees with its own authority, so the
    /// set-equality check below is the gate that keeps them honest.
    #[tokio::test]
    async fn fts_prefilter_is_set_identical_to_like_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(
            root.join("corpus.rs"),
            "fn foo_bar() {}\n\
             let Foo = 1;\n\
             const BAR_BAZ = 2;\n\
             let value = cafe_latte();\n\
             let accented = \"café\";\n\
             fn 안녕하세요() {}\n\
             // punctuation a:b and x-y and (z) here\n\
             match thing { OR_AND_NOT => 0 }\n\
             let repeated = foo_bar_foo();\n",
        )
        .await
        .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // Every probe is >= 3 chars (the production FTS threshold): real
        // substrings across case, snake_case, accents, CJK, FTS-syntax chars,
        // and a guaranteed non-match.
        let probes = [
            "foo",
            "Foo",
            "FOO",
            "foo_bar",
            "bar_baz",
            "value",
            "café",
            "안녕하",
            "NOT",
            "a:b",
            "x-y",
            "(z)",
            "OR_",
            "zzz_nomatch",
        ];
        for q in probes {
            let fts = content_rows(&store, q, true).await;
            let like = content_rows(&store, q, false).await;
            assert_eq!(
                fts, like,
                "FTS pre-filter diverged from LIKE-only for {q:?}"
            );
        }
    }

    /// The FTS index is external-content and trigger-maintained, so it must
    /// stay row-for-row in sync with content_lines through both the per-file
    /// delete and the bulk clear path.
    #[tokio::test]
    async fn fts_index_stays_in_sync_with_content_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\nfn beta() {}\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        assert!(content_row_count(&store).await > 0);
        assert_eq!(fts_row_count(&store).await, content_row_count(&store).await);

        // Delete the file's rows via refresh — triggers must purge FTS too.
        tokio::fs::remove_file(&file).await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(content_row_count(&store).await, 0);
        assert_eq!(
            fts_row_count(&store).await,
            0,
            "FTS rows orphaned after delete"
        );
    }

    /// Sub-3-char queries have no trigrams, so production routes them to the
    /// LIKE-only path; the result must equal the LIKE-only scan (a non-empty
    /// 2-char substring still returns its matches, not a silent zero).
    #[tokio::test]
    async fn short_queries_use_like_only_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn fox() {}\nlet ok = 1;\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // "fn" is 2 chars: search_content must gate it onto the LIKE path and
        // still find the line, identical to the LIKE-only scan.
        let via_api = store.search_content("fn", 50, None).await.unwrap();
        let like_only = content_rows(&store, "fn", false).await;
        assert_eq!(via_api.total, like_only.len());
        assert!(via_api.total > 0);
    }

    #[tokio::test]
    async fn reindex_tracks_content_not_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 1);
        assert_eq!(total_matches(&store, "beta").await, 0);

        // Rewrite with different content. Even where the filesystem preserves
        // the modification time, the content hash differs, so the stored
        // symbol set is replaced rather than left stale.
        tokio::fs::write(&file, "fn beta() {}\n").await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 0);
        assert_eq!(total_matches(&store, "beta").await, 1);

        // Re-indexing identical content is a no-op: the same hash skips
        // extraction, so the symbol count stays put with no duplicate rows.
        let before = store.stats().await.unwrap().symbol_count;
        store.index(IndexOptions::default()).await.unwrap();
        let after = store.stats().await.unwrap().symbol_count;
        assert_eq!(before, after);
        assert_eq!(total_matches(&store, "beta").await, 1);
    }

    #[tokio::test]
    async fn refresh_file_reindexes_edited_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // The edit flow: write new bytes, then refresh that one file. The
        // old rows are replaced — not just dropped — so a search finds the
        // new content immediately, with no stale banner.
        tokio::fs::write(&file, "fn beta() {}\n").await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 0);
        let page = store.search_symbols("beta", 50, None).await.unwrap();
        assert_eq!(page.total, 1);
        assert!(!page.stale);
    }

    #[tokio::test]
    async fn refresh_file_drops_rows_for_a_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        tokio::fs::remove_file(&file).await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 0);
        assert_eq!(store.stats().await.unwrap().file_count, 0);
    }

    /// `refresh_file` honors the same domain as a build pass: a file kind
    /// the indexer never covers, or a path under an ignored component,
    /// must not gain rows just because it was edited.
    #[tokio::test]
    async fn refresh_file_skips_files_a_build_would_not_cover() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        let baseline = store.stats().await.unwrap().file_count;

        let notes = root.join("notes.txt");
        tokio::fs::write(&notes, "fn fake() {}\n").await.unwrap();
        store.refresh_files(&[notes]).await.unwrap();

        let generated = root.join("target").join("gen.rs");
        tokio::fs::create_dir_all(generated.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&generated, "fn generated() {}\n")
            .await
            .unwrap();
        store.refresh_files(&[generated]).await.unwrap();

        assert_eq!(store.stats().await.unwrap().file_count, baseline);
        assert_eq!(total_matches(&store, "generated").await, 0);
    }

    /// A narrowed build's scope is durable: after `--lang rust`, editing
    /// a `.py` file must add nothing — the build excluded that language,
    /// and a refresh honoring the full extension table would contradict
    /// the recorded build domain.
    #[tokio::test]
    async fn refresh_honors_the_builds_language_narrowing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        let script = root.join("tool.py");
        tokio::fs::write(&script, "def beta(): pass\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store
            .index(IndexOptions {
                force: false,
                languages: Some(vec![Language::Rust]),
            })
            .await
            .unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 1);
        assert_eq!(total_matches(&store, "beta").await, 0);

        // Edit the .py file: the rust-scoped index must not gain rows.
        tokio::fs::write(&script, "def beta_v2(): pass\n")
            .await
            .unwrap();
        store
            .refresh_files(std::slice::from_ref(&script))
            .await
            .unwrap();
        assert_eq!(total_matches(&store, "beta_v2").await, 0);

        // An in-scope edit still refreshes.
        tokio::fs::write(root.join("lib.rs"), "fn gamma() {}\n")
            .await
            .unwrap();
        store.refresh_files(&[root.join("lib.rs")]).await.unwrap();
        assert_eq!(total_matches(&store, "gamma").await, 1);
    }

    /// An unrestricted build records the unrestricted scope, so an edit
    /// to any indexed-extension file refreshes — including one created
    /// after the build.
    #[tokio::test]
    async fn full_build_scope_covers_every_indexed_extension() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        let script = root.join("tool.py");
        tokio::fs::write(&script, "def beta(): pass\n")
            .await
            .unwrap();
        store.refresh_files(&[script]).await.unwrap();
        assert_eq!(total_matches(&store, "beta").await, 1);
    }

    /// A store that was merely OPENED (any read does this) has no build
    /// marker: a refresh must leave it empty — never grow a partial index
    /// that would then answer authoritatively. `index clear` returns the
    /// store to the same never-built state.
    #[tokio::test]
    async fn refresh_is_inert_without_a_completed_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(store.stats().await.unwrap().symbol_count, 0);

        // Build, then clear: the marker must clear with the rows.
        store.index(IndexOptions::default()).await.unwrap();
        store.clear().await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(store.stats().await.unwrap().symbol_count, 0);
    }

    /// The build scope round-trips through its meta representation.
    #[test]
    fn build_scope_meta_value_round_trips() {
        let all = BuildScope::All;
        assert_eq!(BuildScope::parse(&all.meta_value()), all);

        let narrowed = BuildScope::from_options(&Some(vec![Language::Python, Language::Rust]));
        let parsed = BuildScope::parse(&narrowed.meta_value());
        assert_eq!(parsed, narrowed);
        let exts = parsed.extensions();
        assert!(exts.contains(&"rs") && exts.contains(&"py"));
        assert!(!exts.contains(&"go"));
    }

    #[tokio::test]
    async fn search_reports_stale_after_external_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert!(!store.search_symbols("alpha", 50, None).await.unwrap().stale);

        // An edit the store never saw (external tool, git checkout): the old
        // rows still match the query but must carry the stale marker…
        tokio::fs::write(&file, "fn alpha() { changed() }\n")
            .await
            .unwrap();
        let page = store.search_symbols("alpha", 50, None).await.unwrap();
        assert_eq!(page.total, 1);
        assert!(page.stale);

        // …until the next index pass clears it.
        store.index(IndexOptions::default()).await.unwrap();
        assert!(!store.search_symbols("alpha", 50, None).await.unwrap().stale);
    }

    #[tokio::test]
    async fn search_reports_stale_when_matched_file_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        tokio::fs::remove_file(&file).await.unwrap();
        assert!(store.search_symbols("alpha", 50, None).await.unwrap().stale);
    }

    #[tokio::test]
    async fn stale_only_considers_files_matched_by_the_query() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("a.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("b.rs"), "fn beta() {}\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // Rewriting an unrelated file must not flag pages it doesn't back —
        // and a byte-identical rewrite isn't stale at all (content hash, not
        // mtime, is the currency key).
        tokio::fs::write(root.join("b.rs"), "fn beta_v2() {}\n")
            .await
            .unwrap();
        assert!(!store.search_symbols("alpha", 50, None).await.unwrap().stale);
        tokio::fs::write(root.join("a.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        assert!(!store.search_symbols("alpha", 50, None).await.unwrap().stale);
    }
}
