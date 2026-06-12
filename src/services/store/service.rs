//! The store abstraction every search/index command speaks to.
//!
//! Mirrors the `LspService` split: one trait, two interchangeable
//! implementations chosen once by `use_daemon` above the mode boundary.
//! `DefaultStoreService` opens the SQLite store in-process; the daemon
//! variant lives in `services::daemon_store`. Both must return identical
//! results for the same inputs — anything that diverges is a parity bug.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::error::StoreError;
use crate::models::symbol::{Language, SymbolKind};

use super::index::Store;
use super::types::{
    ContentSearchResult, IndexOptions, IndexStats, SearchPage, StoreConfig, SymbolSearchResult,
};

#[async_trait]
pub trait StoreService: Send + Sync {
    async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind: Option<SymbolKind>,
    ) -> Result<SearchPage<SymbolSearchResult>, StoreError>;

    async fn search_content(
        &self,
        query: &str,
        limit: usize,
        language: Option<Language>,
    ) -> Result<SearchPage<ContentSearchResult>, StoreError>;

    async fn index(&self, options: IndexOptions) -> Result<IndexStats, StoreError>;

    async fn index_status(&self) -> Result<IndexStats, StoreError>;

    async fn index_clear(&self) -> Result<(), StoreError>;

    /// Bring an edited file's index rows in line with the bytes on disk —
    /// re-extracted while the file exists, dropped once it doesn't — so a
    /// write is searchable immediately. Best-effort, and an index that was
    /// never built stays untouched: a project without a store never gains
    /// one just because a file was edited.
    async fn refresh_file(&self, path: &Path) -> Result<(), StoreError>;
}

/// In-process store. The SQLite connection is opened on first use so
/// commands that never touch the store pay nothing for it.
pub struct DefaultStoreService {
    root: PathBuf,
    config: StoreConfig,
    store: OnceCell<Arc<Store>>,
}

impl DefaultStoreService {
    pub fn new(root: &Path, config: StoreConfig) -> Self {
        Self {
            root: root.to_path_buf(),
            config,
            store: OnceCell::new(),
        }
    }

    async fn store(&self) -> Result<&Arc<Store>, StoreError> {
        self.store
            .get_or_try_init(|| async {
                Store::open(&self.root, self.config.clone())
                    .await
                    .map(Arc::new)
            })
            .await
    }

    /// The store for a read query. A *missing* index — no DB file, e.g. a
    /// read-only or never-built project — maps to `NotInitialized` so the
    /// caller falls back to a filesystem scan. A DB that exists but won't
    /// open is a real failure and is surfaced, never silently scanned over.
    async fn store_for_read(&self) -> Result<&Arc<Store>, StoreError> {
        match self.store().await {
            Ok(store) => Ok(store),
            Err(_) if !Store::db_path(&self.root).exists() => Err(StoreError::NotInitialized),
            Err(e) => Err(e),
        }
    }

    /// Flush the write-ahead log — a daemon-idle maintenance step. A no-op
    /// until the store is first opened, so it never materializes a DB just to
    /// checkpoint nothing.
    pub async fn checkpoint(&self) -> Result<(), StoreError> {
        match self.store.get() {
            Some(store) => store.checkpoint().await,
            None => Ok(()),
        }
    }

    /// Evict entries past their TTL — daemon-idle maintenance. A no-op until
    /// the store is first opened.
    pub async fn cleanup_expired(&self) -> usize {
        match self.store.get() {
            Some(store) => store.cleanup_expired().await,
            None => 0,
        }
    }
}

#[async_trait]
impl StoreService for DefaultStoreService {
    async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind: Option<SymbolKind>,
    ) -> Result<SearchPage<SymbolSearchResult>, StoreError> {
        self.store_for_read()
            .await?
            .search_symbols(query, limit, kind)
            .await
    }

    async fn search_content(
        &self,
        query: &str,
        limit: usize,
        language: Option<Language>,
    ) -> Result<SearchPage<ContentSearchResult>, StoreError> {
        self.store_for_read()
            .await?
            .search_content(query, limit, language)
            .await
    }

    async fn index(&self, options: IndexOptions) -> Result<IndexStats, StoreError> {
        self.store().await?.index(options).await
    }

    async fn index_status(&self) -> Result<IndexStats, StoreError> {
        // No DB means an empty index, not an error — report zeros so status
        // works on a read-only or never-built project just like a read does.
        if !Store::db_path(&self.root).exists() {
            return Ok(IndexStats::default());
        }
        self.store().await?.stats().await
    }

    async fn index_clear(&self) -> Result<(), StoreError> {
        self.store().await?.clear().await
    }

    async fn refresh_file(&self, path: &Path) -> Result<(), StoreError> {
        // Don't materialize an index just to refresh one that was never
        // built — a fresh process re-reads the file on its next indexed read.
        if !Store::db_path(&self.root).exists() {
            return Ok(());
        }
        self.store().await?.refresh_file(path).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The no-disk-touch guarantee: refreshing a file in a project whose
    /// index was never built must not create `.symora` or a store DB.
    #[tokio::test]
    async fn refresh_without_a_store_touches_no_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();

        let service = DefaultStoreService::new(root, StoreConfig::default());
        service.refresh_file(&file).await.unwrap();

        assert!(!root.join(".symora").exists());
    }
}
