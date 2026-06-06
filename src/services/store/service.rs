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

    /// Drop a file's rows so an edit isn't served stale. Best-effort:
    /// invalidating an index that was never built is a no-op.
    async fn invalidate_file(&self, path: &Path) -> Result<(), StoreError>;
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

    async fn invalidate_file(&self, path: &Path) -> Result<(), StoreError> {
        // Don't materialize an index just to invalidate one that was never
        // built — a fresh process re-reads the file on its next indexed read.
        if !Store::db_path(&self.root).exists() {
            return Ok(());
        }
        self.store().await?.invalidate_file(path).await;
        Ok(())
    }
}
