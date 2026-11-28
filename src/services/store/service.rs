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
    TreeCurrency,
};

#[async_trait]
pub trait StoreService: Send + Sync {
    /// `within` confines the answer to files at or under these
    /// project-relative paths; empty confines nothing.
    async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind: Option<SymbolKind>,
        language: Option<Language>,
        within: &[PathBuf],
    ) -> Result<SearchPage<SymbolSearchResult>, StoreError>;

    /// Search content rows of exactly the given languages — the domain is
    /// part of the question, enforced in the store query, so rows a
    /// narrowed build indexed outside it never leak into the answer. The
    /// page carries the coverage the rows were read under (same snapshot),
    /// so routing the uncovered remainder to a live read cannot race a
    /// concurrent rebuild.
    async fn search_content(
        &self,
        query: &str,
        limit: usize,
        languages: &[Language],
    ) -> Result<SearchPage<ContentSearchResult>, StoreError>;

    async fn index(&self, options: IndexOptions) -> Result<IndexStats, StoreError>;

    async fn index_status(&self) -> Result<IndexStats, StoreError>;

    /// The languages the index answers authoritatively for. Callers route
    /// on this rather than on how many rows a query happened to return: a
    /// short result from a covered language is a complete answer, while any
    /// result from an uncovered one is a lower bound no matter its size.
    async fn indexed_languages(&self) -> Result<Vec<Language>, StoreError>;

    /// Which languages' files moved since the last completed build read them.
    ///
    /// Stat-only over the build's own scope — no file is opened — and
    /// [`TreeCurrency::Unknown`] wherever the question cannot be settled. It
    /// is what separates an index-backed zero that means "nothing declares
    /// this" from one that means "nothing declared it at build time", so it is
    /// asked only where a zero would otherwise have to be published
    /// unqualified.
    async fn tree_currency(&self) -> Result<TreeCurrency, StoreError>;

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

    /// Give up, rather than finish, the work a budgeted build left behind —
    /// a build stopped short of publishing, or the upkeep after one that
    /// published — as a one-shot process exits
    /// ([`Store::abandon_unfinished_work`]).
    fn abandon_unfinished_work(&self);
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

    /// The store for a read query, never created by one. A *missing* index —
    /// no DB file, e.g. a read-only or never-built project — maps to
    /// `NotInitialized` without opening anything, so the caller falls back to
    /// its live source and the tree gains no `.symora/` for having been read:
    /// only a command whose purpose is to write (a build, an edit) creates
    /// it. A DB that exists but won't open is a real failure and is surfaced,
    /// never silently scanned over.
    async fn store_for_read(&self) -> Result<&Arc<Store>, StoreError> {
        if let Some(store) = self.store.get() {
            return Ok(store);
        }
        if Self::db_is_definitely_absent(&self.root) {
            return Err(StoreError::NotInitialized);
        }
        self.store().await
    }

    /// Whether there is definitely no index file. Only a probe that ANSWERS
    /// counts: `exists()` folds a permission or metadata error into "absent",
    /// which would turn an index nobody can stat into one nobody built —
    /// silently scanning past a real failure, reporting an empty status, or
    /// skipping the refresh that keeps an edit from leaving stale rows behind.
    fn db_is_definitely_absent(root: &std::path::Path) -> bool {
        matches!(Store::db_path(root).try_exists(), Ok(false))
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
}

#[async_trait]
impl StoreService for DefaultStoreService {
    async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind: Option<SymbolKind>,
        language: Option<Language>,
        within: &[PathBuf],
    ) -> Result<SearchPage<SymbolSearchResult>, StoreError> {
        self.store_for_read()
            .await?
            .search_symbols(query, limit, kind, language, within)
            .await
    }

    async fn search_content(
        &self,
        query: &str,
        limit: usize,
        languages: &[Language],
    ) -> Result<SearchPage<ContentSearchResult>, StoreError> {
        self.store_for_read()
            .await?
            .search_content(query, limit, languages)
            .await
    }

    async fn index(&self, options: IndexOptions) -> Result<IndexStats, StoreError> {
        self.store().await?.index(options).await
    }

    async fn indexed_languages(&self) -> Result<Vec<Language>, StoreError> {
        // No DB covers no language, as a store no build completed in does.
        match self.store_for_read().await {
            Ok(store) => store.indexed_languages().await,
            Err(StoreError::NotInitialized) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    async fn tree_currency(&self) -> Result<TreeCurrency, StoreError> {
        // No DB records no fingerprint to compare, as a store no build
        // completed in does.
        match self.store_for_read().await {
            Ok(store) => store.tree_currency().await,
            Err(StoreError::NotInitialized) => Ok(TreeCurrency::Unknown),
            Err(e) => Err(e),
        }
    }

    async fn index_status(&self) -> Result<IndexStats, StoreError> {
        // No DB means an empty index, not an error — report zeros so status
        // works on a read-only or never-built project just like a read does.
        if Self::db_is_definitely_absent(&self.root) {
            return Ok(IndexStats::default());
        }
        self.store().await?.stats().await
    }

    async fn index_clear(&self) -> Result<(), StoreError> {
        self.store().await?.clear().await
    }

    fn abandon_unfinished_work(&self) {
        if let Some(store) = self.store.get() {
            store.abandon_unfinished_work();
        }
    }

    async fn refresh_files(&self, paths: &[PathBuf]) -> Result<(), StoreError> {
        // Don't materialize a DB just to refresh an index that was never
        // built. This existence check is only the no-disk-touch guard; the
        // authoritative never-built gate is the build marker inside the
        // store (`Store::refresh_files`), because any read materializes
        // the DB file without ever building an index.
        if Self::db_is_definitely_absent(&self.root) {
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

    /// A read is no reason to write: every read a project with no index
    /// answers says `NotInitialized` and leaves the tree without a
    /// `.symora`, so a query run in a directory nobody asked symora to write
    /// to writes nothing there.
    #[tokio::test]
    async fn a_read_creates_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();

        let service = DefaultStoreService::new(root, StoreConfig::default());
        assert!(matches!(
            service.search_symbols("alpha", 10, None, None, &[]).await,
            Err(StoreError::NotInitialized)
        ));
        assert!(matches!(
            service.search_content("alpha", 10, &[Language::Rust]).await,
            Err(StoreError::NotInitialized)
        ));
        assert!(service.indexed_languages().await.unwrap().is_empty());
        assert!(matches!(
            service.tree_currency().await.unwrap(),
            TreeCurrency::Unknown
        ));
        assert!(service.index_status().await.unwrap().languages.is_empty());

        assert!(!root.join(".symora").exists());
    }

    /// The porous-guard regression: a DB that merely exists is not a built
    /// index — a build cut off before publishing, a cleared store, or one a
    /// process opened leaves one behind. An edit after that must leave the
    /// store empty and the search path bare (`NotInitialized` → live
    /// fallback), never a 1-file index answering authoritatively for a
    /// never-indexed project.
    #[tokio::test]
    async fn refresh_after_a_read_materialized_store_stays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();

        // A store opened without a build: the DB file exists, and a read
        // through the service finds nothing completed in it.
        drop(Store::open(root, StoreConfig::default()).await.unwrap());
        assert!(Store::db_path(root).exists());
        let service = DefaultStoreService::new(root, StoreConfig::default());
        let read = service.search_symbols("alpha", 10, None, None, &[]).await;
        assert!(matches!(read, Err(StoreError::NotInitialized)));

        // The edit flow refreshes the file — the never-built store must
        // not gain rows from it.
        tokio::fs::write(&file, "fn beta() {}\n").await.unwrap();
        service
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();

        let after = service.search_symbols("beta", 10, None, None, &[]).await;
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
            .search_symbols("beta", 10, None, None, &[])
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(
            service
                .search_symbols("alpha", 10, None, None, &[])
                .await
                .unwrap()
                .total,
            0
        );
    }
}
