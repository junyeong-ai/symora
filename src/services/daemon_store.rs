//! Daemon-backed [`StoreService`]: forwards every store call over the Unix
//! socket to the long-lived `symora daemon`, which owns the SQLite store.
//!
//! The wire carries the same JSON the CLI emits, so responses parse back
//! into the typed `SearchPage`/`IndexStats` the trait promises. The
//! "store not yet indexed" signal travels as a dedicated error code, so the
//! command layer can branch on the typed `StoreError::NotInitialized`
//! rather than matching on a message string.

use std::path::Path;

use async_trait::async_trait;

use crate::cli::response::Section;
use crate::daemon::DaemonClient;
use crate::error::{LspError, StoreError};
use crate::models::symbol::{Language, SymbolKind};
use crate::services::store::{
    ContentSearchResult, IndexOptions, IndexStats, SearchPage, StoreService, SymbolSearchResult,
};

pub struct DaemonStoreService {
    client: DaemonClient,
}

impl DaemonStoreService {
    pub fn new(root: &Path) -> Self {
        Self {
            client: DaemonClient::new(root),
        }
    }
}

#[async_trait]
impl StoreService for DaemonStoreService {
    async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind: Option<SymbolKind>,
        language: Option<Language>,
    ) -> Result<SearchPage<SymbolSearchResult>, StoreError> {
        let kind = kind.map(|k| k.to_string());
        let language = language.map(|l| l.lsp_id().to_string());
        let response = self
            .client
            .search_symbols(query, Some(limit), kind.as_deref(), language.as_deref())
            .await
            .map_err(store_error)?;
        let section: Section<SymbolSearchResult> =
            serde_json::from_value(response).map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(SearchPage {
            total: section.count,
            rows: section.items,
            stale: section.stale,
        })
    }

    async fn search_content(
        &self,
        query: &str,
        limit: usize,
        language: Option<Language>,
    ) -> Result<SearchPage<ContentSearchResult>, StoreError> {
        let language = language.map(|l| l.lsp_id().to_string());
        let response = self
            .client
            .search_content(query, Some(limit), language.as_deref())
            .await
            .map_err(store_error)?;
        let section: Section<ContentSearchResult> =
            serde_json::from_value(response).map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(SearchPage {
            total: section.count,
            rows: section.items,
            stale: section.stale,
        })
    }

    async fn index(&self, options: IndexOptions) -> Result<IndexStats, StoreError> {
        let languages = options
            .languages
            .map(|langs| langs.iter().map(|l| l.lsp_id().to_string()).collect());
        let response = self
            .client
            .index_build(options.force, languages)
            .await
            .map_err(store_error)?;
        serde_json::from_value(response).map_err(|e| StoreError::Database(e.to_string()))
    }

    async fn index_status(&self) -> Result<IndexStats, StoreError> {
        let response = self.client.index_status().await.map_err(store_error)?;
        serde_json::from_value(response).map_err(|e| StoreError::Database(e.to_string()))
    }

    async fn index_clear(&self) -> Result<(), StoreError> {
        self.client.index_clear().await.map_err(store_error)?;
        Ok(())
    }

    async fn refresh_files(&self, paths: &[std::path::PathBuf]) -> Result<(), StoreError> {
        self.client
            .refresh_files(paths)
            .await
            .map_err(store_error)?;
        Ok(())
    }
}

/// Recover the typed "not indexed yet" signal from the wire error code so
/// callers branch on `StoreError::NotInitialized`, never on a message.
fn store_error(error: LspError) -> StoreError {
    let code = error.error_code();
    if code == StoreError::NOT_INITIALIZED_CODE {
        StoreError::NotInitialized
    } else if code == StoreError::ALREADY_INDEXING_CODE {
        StoreError::AlreadyIndexing
    } else {
        StoreError::Database(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The semantic store errors must survive the wire round-trip by code so
    /// the daemon path returns the same typed variant as the direct path.
    #[test]
    fn store_error_reconstructs_semantic_variants_from_wire_code() {
        let from_code =
            |code| store_error(LspError::server_error_friendly(code, "ignored".to_string()));
        assert!(matches!(
            from_code(StoreError::NOT_INITIALIZED_CODE),
            StoreError::NotInitialized
        ));
        assert!(matches!(
            from_code(StoreError::ALREADY_INDEXING_CODE),
            StoreError::AlreadyIndexing
        ));
        assert!(matches!(from_code(-32603), StoreError::Database(_)));
    }

    /// The `stale` marker must survive the daemon wire so the daemon path
    /// reports the same staleness as the direct path (INV3). This pins both
    /// ends: the daemon serializes `Section::with_stale`, the client
    /// reconstructs `SearchPage { stale }` from it.
    #[test]
    fn stale_marker_survives_the_wire_round_trip() {
        // Daemon end: a stale page is emitted exactly as store_handlers does.
        let emitted = Section::with_total(Vec::<SymbolSearchResult>::new(), 3).with_stale(true);
        let wire = serde_json::to_value(emitted).unwrap();

        // Client end: reconstruct as DaemonStoreService::search_symbols does.
        let section: Section<SymbolSearchResult> = serde_json::from_value(wire).unwrap();
        let page = SearchPage {
            total: section.count,
            rows: section.items,
            stale: section.stale,
        };
        assert!(page.stale);
        assert_eq!(page.total, 3);

        // A fresh page omits the field on the wire and reconstructs as false.
        let fresh_wire =
            serde_json::to_value(Section::with_total(Vec::<SymbolSearchResult>::new(), 0)).unwrap();
        assert!(fresh_wire.get("stale").is_none());
        let fresh: Section<SymbolSearchResult> = serde_json::from_value(fresh_wire).unwrap();
        assert!(!fresh.stale);
    }
}
