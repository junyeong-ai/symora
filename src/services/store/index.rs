use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::db::SqliteDb;
use super::db::rusqlite;
use super::schema::*;
use super::symbols::SymbolExtractor;
use super::types::*;
use crate::error::StoreError;
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::{Language, SymbolKind};

pub struct Store {
    db: SqliteDb,
    project_root: PathBuf,
    config: StoreConfig,
    symbol_extractor: SymbolExtractor,
    is_indexing: AtomicBool,
    index_ready: AtomicBool,
    next_file_id: AtomicI64,
    next_symbol_id: AtomicI64,
    next_content_id: AtomicI64,
}

impl Store {
    pub async fn open(project_root: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        let db_path = project_root.join(".symora").join("store.db");
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(StoreError::Io)?;
        }

        let db = match Self::try_open_db(&db_path).await {
            Ok(db) => db,
            Err(_) => {
                tracing::warn!(
                    "Store database corrupted, recreating: {}",
                    db_path.display()
                );
                Self::recover_db(&db_path).await?
            }
        };

        let (next_file_id, next_symbol_id, next_content_id, has_data) = db
            .call(|conn| {
                Ok((
                    conn.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM files", [], |r| {
                        r.get(0)
                    })
                    .unwrap_or(1),
                    conn.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM symbols", [], |r| {
                        r.get(0)
                    })
                    .unwrap_or(1),
                    conn.query_row(
                        "SELECT COALESCE(MAX(id), 0) + 1 FROM content_lines",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(1),
                    conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                        .unwrap_or(0)
                        > 0,
                ))
            })
            .await?;

        Ok(Self {
            db,
            project_root: project_root.to_path_buf(),
            config,
            symbol_extractor: SymbolExtractor::new(),
            is_indexing: AtomicBool::new(false),
            index_ready: AtomicBool::new(has_data),
            next_file_id: AtomicI64::new(next_file_id),
            next_symbol_id: AtomicI64::new(next_symbol_id),
            next_content_id: AtomicI64::new(next_content_id),
        })
    }

    async fn try_open_db(db_path: &Path) -> Result<SqliteDb, StoreError> {
        let db = SqliteDb::open(db_path).await?;
        db.execute(INIT_SCHEMA).await?;
        db.call(|conn| {
            conn.execute("SELECT 1 FROM files LIMIT 1", [])?;
            Ok(())
        })
        .await?;
        Ok(db)
    }

    async fn recover_db(db_path: &Path) -> Result<SqliteDb, StoreError> {
        if db_path.exists() {
            let backup_path = db_path.with_extension("db.bak");
            let _ = tokio::fs::rename(db_path, &backup_path).await;
        }
        let db = SqliteDb::open(db_path).await?;
        db.execute(INIT_SCHEMA).await?;
        Ok(db)
    }

    pub fn is_indexing(&self) -> bool {
        self.is_indexing.load(Ordering::SeqCst)
    }

    // ============ Search Operations ============

    pub async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind_filter: Option<SymbolKind>,
    ) -> Result<Vec<SymbolSearchResult>, StoreError> {
        if !self.index_ready.load(Ordering::SeqCst) {
            return Err(StoreError::NotInitialized);
        }

        let query = query.to_string();
        let limit = limit as i64;
        let kind_str = kind_filter.map(|k| k.to_string());

        self.db
            .call(move |conn| {
                let sql = if kind_str.is_some() {
                    SEARCH_SYMBOLS_WITH_KIND_QUERY
                } else {
                    SEARCH_SYMBOLS_QUERY
                };
                let mut stmt = conn.prepare(sql)?;
                let rows = match &kind_str {
                    Some(k) => stmt.query(rusqlite::params![query, limit, k])?,
                    None => stmt.query(rusqlite::params![query, limit])?,
                };

                Ok(rows
                    .mapped(|r| {
                        Ok(SymbolSearchResult {
                            name: r.get(0)?,
                            kind: SymbolKind::from_str_loose(&r.get::<_, String>(1)?),
                            line: r.get::<_, i32>(2)? as u32,
                            column: r.get::<_, i32>(3)? as u32,
                            file: PathBuf::from(r.get::<_, String>(4)?),
                            score: r.get(5)?,
                        })
                    })
                    .filter_map(|r| r.ok())
                    .collect())
            })
            .await
    }

    pub async fn search_content(
        &self,
        query: &str,
        limit: usize,
        language: Option<Language>,
    ) -> Result<Vec<ContentSearchResult>, StoreError> {
        if !self.index_ready.load(Ordering::SeqCst) {
            return Err(StoreError::NotInitialized);
        }

        let query = query.to_string();
        let limit = limit as i64;
        let lang_str = language.map(|l| l.lsp_id().to_string());

        self.db
            .call(move |conn| {
                let sql = if lang_str.is_some() {
                    SEARCH_CONTENT_WITH_LANG_QUERY
                } else {
                    SEARCH_CONTENT_QUERY
                };
                let mut stmt = conn.prepare(sql)?;
                let rows = match &lang_str {
                    Some(l) => stmt.query(rusqlite::params![query, limit, l])?,
                    None => stmt.query(rusqlite::params![query, limit])?,
                };

                Ok(rows
                    .mapped(|r| {
                        Ok(ContentSearchResult {
                            content: r.get(0)?,
                            line: r.get::<_, i32>(1)? as u32,
                            file: PathBuf::from(r.get::<_, String>(2)?),
                            score: r.get(4)?,
                        })
                    })
                    .filter_map(|r| r.ok())
                    .collect())
            })
            .await
    }

    // ============ Maintenance Operations ============

    pub async fn invalidate_file(&self, path: &Path) {
        let path_str = path.display().to_string();
        let _ = self
            .db
            .call(move |conn| {
                let file_id: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM files WHERE path = ?1",
                        rusqlite::params![&path_str],
                        |r| r.get(0),
                    )
                    .ok();

                if let Some(fid) = file_id {
                    delete_file_cascade(conn, fid)?;
                }
                Ok(())
            })
            .await;
    }

    pub async fn cleanup_expired(&self) -> usize {
        let cutoff = (now_unix() - self.config.ttl_secs) as i64;
        self.db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let count: usize = tx.query_row(
                    "SELECT COUNT(*) FROM files WHERE indexed_at < ?1",
                    rusqlite::params![cutoff],
                    |r| r.get(0),
                )?;

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

    pub async fn clear(&self) -> Result<(), StoreError> {
        self.db
            .execute("DELETE FROM content_lines; DELETE FROM symbols; DELETE FROM files; VACUUM;")
            .await?;
        self.next_file_id.store(1, Ordering::SeqCst);
        self.next_symbol_id.store(1, Ordering::SeqCst);
        self.next_content_id.store(1, Ordering::SeqCst);
        self.index_ready.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub async fn stats(&self) -> Result<IndexStats, StoreError> {
        let db_path = self.project_root.join(".symora").join("store.db");
        let index_size_bytes = tokio::fs::metadata(&db_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let is_indexing = self.is_indexing.load(Ordering::SeqCst);

        self.db
            .call(move |conn| {
                Ok(IndexStats {
                    file_count: conn
                        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                        .unwrap_or(0),
                    symbol_count: conn
                        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
                        .unwrap_or(0),
                    content_line_count: conn
                        .query_row("SELECT COUNT(*) FROM content_lines", [], |r| r.get(0))
                        .unwrap_or(0),
                    index_size_bytes,
                    last_indexed: conn
                        .query_row("SELECT COALESCE(MAX(indexed_at), 0) FROM files", [], |r| {
                            r.get(0)
                        })
                        .unwrap_or(0),
                    is_indexing,
                    progress: None,
                })
            })
            .await
    }

    // ============ Indexing Operations ============

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
        let extensions: Vec<&str> = options
            .languages
            .as_ref()
            .map(|langs| langs.iter().flat_map(|l| l.extensions()).copied().collect())
            .unwrap_or_else(|| {
                vec![
                    "rs", "go", "py", "ts", "tsx", "js", "jsx", "java", "kt", "kts", "cpp", "cc",
                    "cxx", "c", "h", "hpp", "cs", "rb", "php", "lua", "sh",
                ]
            });

        let files = filter.discover_files(&extensions);

        if options.force {
            self.clear().await?;
        }

        for file in &files {
            self.index_file(file).await?;
        }

        self.index_ready.store(true, Ordering::SeqCst);
        self.stats().await
    }

    async fn index_file(&self, path: &Path) -> Result<(), StoreError> {
        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            Err(_) => return Ok(()),
        };

        let mtime = tokio::fs::metadata(path)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;

        let language = Language::from_path(path);
        let lang_str = match language {
            Language::Unknown => None,
            l => Some(l.lsp_id().to_string()),
        };

        let file_path = path.display().to_string();
        let new_file_id = self.next_file_id.fetch_add(1, Ordering::SeqCst);
        let now = now_unix() as i64;

        let file_id = self.db
            .call(move |conn| {
                let existing_id: Option<i64> = conn
                    .query_row("SELECT id FROM files WHERE path = ?1", rusqlite::params![file_path.clone()], |r| r.get(0))
                    .ok();

                match existing_id {
                    Some(id) => {
                        conn.execute("UPDATE files SET mtime = ?1, language = ?2, indexed_at = ?3 WHERE id = ?4", rusqlite::params![mtime, lang_str, now, id])?;
                        delete_file_data(conn, id)?;
                        Ok(id)
                    }
                    None => {
                        conn.execute(
                            "INSERT INTO files (id, path, mtime, language, indexed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                            rusqlite::params![new_file_id, file_path, mtime, lang_str, now],
                        )?;
                        Ok(new_file_id)
                    }
                }
            })
            .await?;

        self.index_symbols(file_id, &content, language).await?;

        if self.config.index_content {
            self.index_content_lines(file_id, &content).await?;
        }

        Ok(())
    }

    async fn index_symbols(
        &self,
        file_id: i64,
        content: &str,
        language: Language,
    ) -> Result<(), StoreError> {
        let extracted = self.symbol_extractor.extract(content, language);
        if extracted.is_empty() {
            return Ok(());
        }

        let symbols: Vec<(i64, i64, String, String, i32, i32)> = extracted
            .into_iter()
            .map(|s| {
                (
                    self.next_symbol_id.fetch_add(1, Ordering::SeqCst),
                    file_id,
                    s.name,
                    s.kind.to_string(),
                    s.line as i32,
                    s.column as i32,
                )
            })
            .collect();

        self.db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                {
                    let mut stmt = tx.prepare("INSERT INTO symbols (id, file_id, name, kind, line, col) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
                    for (id, file_id, name, kind, line, col) in &symbols {
                        stmt.execute(rusqlite::params![id, file_id, name, kind, line, col])?;
                    }
                }
                tx.commit()
            })
            .await
    }

    async fn index_content_lines(&self, file_id: i64, content: &str) -> Result<(), StoreError> {
        let lines: Vec<(i64, i32, String)> = content
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(i, line)| {
                (
                    self.next_content_id.fetch_add(1, Ordering::SeqCst),
                    (i + 1) as i32,
                    line.to_string(),
                )
            })
            .collect();

        self.db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                {
                    let mut stmt = tx.prepare("INSERT INTO content_lines (id, file_id, line_num, content) VALUES (?1, ?2, ?3, ?4)")?;
                    for (id, line_num, content) in &lines {
                        stmt.execute(rusqlite::params![id, file_id, line_num, content])?;
                    }
                }
                tx.commit()
            })
            .await
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn delete_file_data(conn: &rusqlite::Connection, file_id: i64) -> Result<(), rusqlite::Error> {
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

fn delete_file_cascade(conn: &rusqlite::Connection, file_id: i64) -> Result<(), rusqlite::Error> {
    delete_file_data(conn, file_id)?;
    conn.execute(
        "DELETE FROM files WHERE id = ?1",
        rusqlite::params![file_id],
    )?;
    Ok(())
}
