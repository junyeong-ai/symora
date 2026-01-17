//! Async SQLite FTS5 database wrapper

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_rusqlite::Connection;

use super::schema::{
    SCHEMA, SEARCH_CONTENT_QUERY, SEARCH_CONTENT_WITH_LANG_QUERY, SEARCH_SYMBOLS_QUERY,
};
use super::types::{ContentSearchResult, IndexStats, SymbolIndexEntry, SymbolSearchResult};
use crate::error::SearchError;
use crate::models::symbol::{Language, SymbolKind};

pub struct SearchDb {
    conn: Connection,
}

impl SearchDb {
    pub async fn open(path: &Path) -> Result<Self, SearchError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(SearchError::Io)?;
        }

        let conn = Connection::open(path)
            .await
            .map_err(|e| SearchError::Database(e.to_string()))?;

        conn.call(|conn| -> Result<(), rusqlite::Error> {
            conn.execute_batch(SCHEMA)?;
            Ok(())
        })
        .await
        .map_err(|e| SearchError::Database(e.to_string()))?;

        Ok(Self { conn })
    }

    pub async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind_filter: Option<&[SymbolKind]>,
    ) -> Result<Vec<SymbolSearchResult>, SearchError> {
        let query = escape_fts_query(query);
        let limit = limit as i64;

        let results = self
            .conn
            .call(
                move |conn| -> Result<Vec<SymbolSearchResult>, rusqlite::Error> {
                    let mut stmt = conn.prepare(SEARCH_SYMBOLS_QUERY)?;
                    stmt.query_map(rusqlite::params![query, limit], |row| {
                        Ok(SymbolSearchResult {
                            name: row.get(0)?,
                            kind: SymbolKind::from_str_loose(row.get::<_, String>(1)?.as_str()),
                            container: row.get(2)?,
                            line: row.get(3)?,
                            column: row.get(4)?,
                            file: PathBuf::from(row.get::<_, String>(5)?),
                            score: row.get::<_, f64>(6)?.abs(),
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()
                },
            )
            .await
            .map_err(|e| SearchError::Database(e.to_string()))?;

        match kind_filter {
            Some(kinds) if !kinds.is_empty() => Ok(results
                .into_iter()
                .filter(|r| kinds.contains(&r.kind))
                .collect()),
            _ => Ok(results),
        }
    }

    pub async fn search_content(
        &self,
        query: &str,
        limit: usize,
        language: Option<Language>,
    ) -> Result<Vec<ContentSearchResult>, SearchError> {
        let query = escape_fts_query(query);
        let limit = limit as i64;
        let lang_str = language.map(|l| l.lsp_id().to_string());

        self.conn
            .call(
                move |conn| -> Result<Vec<ContentSearchResult>, rusqlite::Error> {
                    let row_mapper = |row: &rusqlite::Row| {
                        Ok(ContentSearchResult {
                            content: row.get(0)?,
                            line: row.get(1)?,
                            file: PathBuf::from(row.get::<_, String>(2)?),
                            score: row.get::<_, f64>(4)?.abs(),
                        })
                    };

                    let rows: Vec<ContentSearchResult> = match &lang_str {
                        Some(lang) => {
                            let mut stmt = conn.prepare(SEARCH_CONTENT_WITH_LANG_QUERY)?;
                            stmt.query_map(rusqlite::params![query, lang, limit], row_mapper)?
                                .collect::<Result<Vec<_>, _>>()?
                        }
                        None => {
                            let mut stmt = conn.prepare(SEARCH_CONTENT_QUERY)?;
                            stmt.query_map(rusqlite::params![query, limit], row_mapper)?
                                .collect::<Result<Vec<_>, _>>()?
                        }
                    };

                    Ok(rows)
                },
            )
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn get_or_create_file(
        &self,
        path: &Path,
        mtime: u64,
        language: Option<Language>,
    ) -> Result<i64, SearchError> {
        let path_str = path.display().to_string();
        let lang_str = language.map(|l| l.lsp_id().to_string());
        let now = now_unix();

        self.conn
            .call(move |conn| -> Result<i64, rusqlite::Error> {
                conn.execute(
                    "INSERT OR REPLACE INTO files (path, mtime, language, indexed_at) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![path_str, mtime as i64, lang_str, now as i64],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn needs_reindex(
        &self,
        path: &Path,
        current_mtime: u64,
    ) -> Result<bool, SearchError> {
        let path_str = path.display().to_string();

        self.conn
            .call(move |conn| -> Result<bool, rusqlite::Error> {
                let stored_mtime: Option<i64> = conn
                    .query_row(
                        "SELECT mtime FROM files WHERE path = ?1",
                        rusqlite::params![path_str],
                        |row| row.get(0),
                    )
                    .ok();

                Ok(stored_mtime.map(|m| m as u64) != Some(current_mtime))
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn delete_file(&self, path: &Path) -> Result<(), SearchError> {
        let path_str = path.display().to_string();

        self.conn
            .call(move |conn| -> Result<(), rusqlite::Error> {
                conn.execute(
                    "DELETE FROM files WHERE path = ?1",
                    rusqlite::params![path_str],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn insert_symbols(
        &self,
        file_id: i64,
        symbols: Vec<SymbolIndexEntry>,
    ) -> Result<(), SearchError> {
        self.conn
            .call(move |conn| -> Result<(), rusqlite::Error> {
                let tx = conn.transaction()?;

                {
                    let mut stmt = tx.prepare(
                        "INSERT INTO symbols (file_id, name, kind, container, line, col) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )?;

                    for sym in &symbols {
                        stmt.execute(rusqlite::params![
                            file_id,
                            sym.name,
                            sym.kind.to_string(),
                            sym.container,
                            sym.line as i64,
                            sym.column as i64
                        ])?;
                    }
                }

                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn insert_content_lines(
        &self,
        file_id: i64,
        lines: Vec<(u32, String)>,
    ) -> Result<(), SearchError> {
        self.conn
            .call(move |conn| -> Result<(), rusqlite::Error> {
                let tx = conn.transaction()?;

                {
                    let mut stmt = tx.prepare(
                        "INSERT INTO content_lines (file_id, line_num, content) VALUES (?1, ?2, ?3)",
                    )?;

                    for (line_num, content) in &lines {
                        if !content.trim().is_empty() {
                            stmt.execute(rusqlite::params![file_id, *line_num as i64, content])?;
                        }
                    }
                }

                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn stats(&self) -> Result<IndexStats, SearchError> {
        self.conn
            .call(|conn| -> Result<IndexStats, rusqlite::Error> {
                let file_count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
                let symbol_count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))?;
                let content_count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM content_lines", [], |row| row.get(0))?;
                let last_indexed: i64 = conn
                    .query_row(
                        "SELECT COALESCE(MAX(indexed_at), 0) FROM files",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);

                Ok(IndexStats {
                    file_count: file_count as usize,
                    symbol_count: symbol_count as usize,
                    content_line_count: content_count as usize,
                    index_size_bytes: 0,
                    last_indexed: last_indexed as u64,
                    is_indexing: false,
                    progress: None,
                })
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn clear(&self) -> Result<(), SearchError> {
        self.conn
            .call(|conn| -> Result<(), rusqlite::Error> {
                conn.execute_batch(
                    "DELETE FROM content_lines; DELETE FROM symbols; DELETE FROM files;",
                )?;
                Ok(())
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn cleanup_expired(&self, ttl_secs: u64) -> Result<usize, SearchError> {
        let cutoff = now_unix().saturating_sub(ttl_secs);

        self.conn
            .call(move |conn| -> Result<usize, rusqlite::Error> {
                let deleted = conn.execute(
                    "DELETE FROM files WHERE indexed_at < ?1",
                    rusqlite::params![cutoff as i64],
                )?;
                Ok(deleted)
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }

    pub async fn optimize(&self) -> Result<(), SearchError> {
        self.conn
            .call(|conn| -> Result<(), rusqlite::Error> {
                conn.execute(
                    "INSERT INTO symbols_fts(symbols_fts) VALUES('optimize')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO content_fts(content_fts) VALUES('optimize')",
                    [],
                )?;
                conn.execute("VACUUM", [])?;
                Ok(())
            })
            .await
            .map_err(|e| SearchError::Database(e.to_string()))
    }
}

fn escape_fts_query(query: &str) -> String {
    let mut result = String::with_capacity(query.len() * 2);
    for ch in query.chars() {
        match ch {
            '"' | '\'' | '(' | ')' | '*' | ':' | '^' | '-' | '+' => {
                result.push(' ');
            }
            _ => result.push(ch),
        }
    }
    result.trim().to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_fts_query_removes_special_chars() {
        assert_eq!(escape_fts_query("hello world"), "hello world");
        assert_eq!(escape_fts_query("foo*bar"), "foo bar");
        assert_eq!(escape_fts_query("test:value"), "test value");
        assert_eq!(escape_fts_query("a+b-c"), "a b c");
        assert_eq!(escape_fts_query("(func)"), "func");
        assert_eq!(escape_fts_query("\"quoted\""), "quoted");
        assert_eq!(escape_fts_query("prefix^suffix"), "prefix suffix");
    }

    #[test]
    fn escape_fts_query_trims_whitespace() {
        assert_eq!(escape_fts_query("  hello  "), "hello");
        assert_eq!(escape_fts_query("*test*"), "test");
    }

    #[test]
    fn escape_fts_query_handles_empty() {
        assert_eq!(escape_fts_query(""), "");
        assert_eq!(escape_fts_query("   "), "");
        assert_eq!(escape_fts_query("***"), "");
    }

    #[test]
    fn escape_fts_query_preserves_underscores() {
        assert_eq!(escape_fts_query("snake_case"), "snake_case");
        assert_eq!(escape_fts_query("__init__"), "__init__");
    }
}
