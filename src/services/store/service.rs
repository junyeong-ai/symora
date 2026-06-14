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
        language: Option<Language>,
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

    /// Bring just-edited files' index rows in line with the bytes on disk
    /// — re-extracted while a file exists, dropped once it doesn't — so a
    /// write is searchable immediately. One call covers a whole edit batch
    /// (rename and actions touch many files), so ignore rules are built
    /// once per batch. Best-effort from the edit's point of view (a
    /// returned error is logged, never fails the edit), and an index that
    /// was never *built* stays untouched: neither a project without a
    /// store nor a store materialized by a mere read gains rows because a
    /// file was edited — the persisted build marker decides, not file
    /// existence.
    async fn refresh_files(&self, paths: &[PathBuf]) -> Result<(), StoreError>;
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
        language: Option<Language>,
    ) -> Result<SearchPage<SymbolSearchResult>, StoreError> {
        self.store_for_read()
            .await?
            .search_symbols(query, limit, kind, language)
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

    async fn refresh_files(&self, paths: &[PathBuf]) -> Result<(), StoreError> {
        // Don't materialize a DB just to refresh an index that was never
        // built. This existence check is only the no-disk-touch guard; the
        // authoritative never-built gate is the build marker inside the
        // store (`Store::refresh_files`), because any read materializes
        // the DB file without ever building an index.
        if !Store::db_path(&self.root).exists() {
            return Ok(());
        }
        self.store().await?.refresh_files(paths).await
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
        service
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();

        assert!(!root.join(".symora").exists());
    }

    /// The porous-guard regression: ANY read materializes the DB file
    /// (`store_for_read` → `Store::open`), so a DB that merely exists is
    /// not a built index. An edit after such a read must leave the store
    /// empty and the search path bare (`NotInitialized` → live fallback),
    /// never a 1-file index answering authoritatively for a never-indexed
    /// project.
    #[tokio::test]
    async fn refresh_after_a_read_materialized_store_stays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();

        let service = DefaultStoreService::new(root, StoreConfig::default());

        // A read on a never-built project: NotInitialized, but the DB file
        // now exists on disk.
        let read = service.search_symbols("alpha", 10, None, None).await;
        assert!(matches!(read, Err(StoreError::NotInitialized)));
        assert!(Store::db_path(root).exists());

        // The edit flow refreshes the file — the never-built store must
        // not gain rows from it.
        tokio::fs::write(&file, "fn beta() {}\n").await.unwrap();
        service
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();

        let after = service.search_symbols("beta", 10, None, None).await;
        assert!(
            matches!(after, Err(StoreError::NotInitialized)),
            "a read-materialized store must stay never-built after an edit"
        );
        assert_eq!(service.index_status().await.unwrap().symbol_count, 0);
    }

    /// After a real build, the refresh path works exactly as before.
    #[tokio::test]
    async fn refresh_after_a_full_build_reindexes_the_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();

        let service = DefaultStoreService::new(root, StoreConfig::default());
        service.index(IndexOptions::default()).await.unwrap();

        tokio::fs::write(&file, "fn beta() {}\n").await.unwrap();
        service
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();

        let page = service
            .search_symbols("beta", 10, None, None)
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(
            service
                .search_symbols("alpha", 10, None, None)
                .await
                .unwrap()
                .total,
            0
        );
    }
}
