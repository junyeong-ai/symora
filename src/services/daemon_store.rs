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

/// Rebuild a store page from the daemon's answer. Rows and coverage cross
/// the wire together because they were read together.
fn parse_page<T: serde::de::DeserializeOwned>(
    response: serde_json::Value,
) -> Result<SearchPage<T>, StoreError> {
    let response: crate::daemon::wire::SearchResponse<T> =
        serde_json::from_value(response).map_err(|e| StoreError::Database(e.to_string()))?;
    Ok(SearchPage {
        total: response.count,
        rows: response.items,
        stale_files: response.stale_files,
        covered: response
            .covered
            .iter()
            .map(|l| Language::parse_or_default(l))
            .collect(),
        unread_paths: response.unread_paths,
    })
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
        parse_page(response)
    }

    async fn search_content(
        &self,
        query: &str,
        limit: usize,
        languages: &[Language],
    ) -> Result<SearchPage<ContentSearchResult>, StoreError> {
        let languages: Vec<String> = languages.iter().map(|l| l.lsp_id().to_string()).collect();
        let response = self
            .client
            .search_content(query, Some(limit), &languages)
            .await
            .map_err(store_error)?;
        parse_page(response)
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

    async fn indexed_languages(&self) -> Result<Vec<Language>, StoreError> {
        let response = self.client.indexed_languages().await.map_err(store_error)?;
        serde_json::from_value(response).map_err(|e| StoreError::Database(e.to_string()))
    }

    async fn index_is_current(&self) -> Result<bool, StoreError> {
        let response = self.client.index_is_current().await.map_err(store_error)?;
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

/// Recover the typed store error from the wire code so callers branch on
/// `StoreError::NotInitialized` and friends, never on a message. The
/// message is the one the daemon rendered, taken raw rather than through
/// the transport error's `Display`, so a variant carrying it renders
/// exactly what the direct path renders instead of restating it inside
/// another layer of framing.
fn store_error(error: LspError) -> StoreError {
    // Only an answer from the daemon describes the store. Anything else —
    // a socket that closed, a request that never returned — is the
    // transport failing, and disguising it as a store error would relabel
    // one event by whichever command happened to be in flight.
    let LspError::ServerError { code, message } = error else {
        return StoreError::Unreachable(Box::new(error));
    };
    if code == StoreError::NOT_INITIALIZED_CODE {
        StoreError::NotInitialized
    } else if code == StoreError::BUSY_CODE {
        StoreError::Busy
    } else if code == StoreError::ALREADY_INDEXING_CODE {
        StoreError::AlreadyIndexing
    } else if code == StoreError::REBUILDING_CODE {
        StoreError::Rebuilding
    } else if code == StoreError::EMPTY_SCOPE_CODE {
        StoreError::EmptyScope
    } else if code == StoreError::IO_CODE {
        StoreError::Io(std::io::Error::other(message))
    } else {
        StoreError::Database(message)
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

    /// INV3 made executable at the store boundary: whatever a command
    /// reports for a store failure directly, it must report — code and
    /// message both — for the same failure through the daemon. A variant
    /// with no wire code of its own arrives as `Database`, which renders
    /// verbatim; one that needs a different output code or a different
    /// rendering must carry its own code, and fails here until it does.
    /// `Unreachable` is absent by construction: it says the daemon was not
    /// reached, so it is never something a daemon sends.
    #[test]
    fn every_store_error_reads_the_same_across_the_wire() {
        use crate::cli::OutputError;
        use crate::daemon::DaemonClient;
        use crate::daemon::protocol::{RequestId, Response, RpcError};

        let variants: Vec<fn() -> StoreError> = vec![
            || StoreError::Database("boom".to_string()),
            || StoreError::Corrupt("boom".to_string()),
            || StoreError::SchemaMismatch {
                found: 1,
                expected: 2,
            },
            || StoreError::NotInitialized,
            || StoreError::AlreadyIndexing,
            || StoreError::Busy,
            || StoreError::Rebuilding,
            || StoreError::EmptyScope,
            || StoreError::Io(std::io::Error::other("boom")),
        ];

        // Three things keep the list from drifting from the enum, none of
        // them the list itself: the match below is exhaustive, so a new
        // variant fails to compile here; `From<StoreError> for RpcError` has
        // no catch-all, so it fails to compile there until someone decides its
        // wire code; and the count comes from `StoreError` rather than from
        // this list, which could only ever agree with itself.
        let mut sampled = std::collections::HashSet::new();
        for make in &variants {
            let name = match make() {
                StoreError::Database(_) => "Database",
                StoreError::Corrupt(_) => "Corrupt",
                StoreError::SchemaMismatch { .. } => "SchemaMismatch",
                StoreError::NotInitialized => "NotInitialized",
                StoreError::AlreadyIndexing => "AlreadyIndexing",
                StoreError::Busy => "Busy",
                StoreError::Rebuilding => "Rebuilding",
                StoreError::EmptyScope => "EmptyScope",
                StoreError::Io(_) => "Io",
                StoreError::Unreachable(_) => {
                    unreachable!("a daemon cannot fail to reach its own store")
                }
            };
            assert!(sampled.insert(name), "{name} is sampled twice");
        }
        assert_eq!(
            sampled.len(),
            StoreError::SENDABLE_VARIANTS,
            "every variant a daemon can send must be sampled: {sampled:?}"
        );

        for make in variants {
            let direct = OutputError::from(make());
            let response = Response::error(RequestId::Number(1), RpcError::from(make()));
            let transport = DaemonClient::extract_result(response)
                .expect_err("an error response must not read as a result");
            let through_daemon = OutputError::from(store_error(transport));
            assert_eq!(
                direct.code, through_daemon.code,
                "{} diverges in code between direct and daemon mode",
                direct.message
            );
            assert_eq!(
                direct.message, through_daemon.message,
                "the same failure reads differently through the daemon"
            );
        }
    }

    /// A transport that failed is reported as itself. The same lost daemon
    /// must read the same whichever command was in flight, so the store
    /// path may not relabel it by the subsystem that happened to be
    /// asking — an `internal` here would also contradict its own message,
    /// which prescribes the retry that `internal` denies.
    #[test]
    fn a_lost_daemon_reads_the_same_on_the_store_path_as_on_the_lsp_path() {
        use crate::cli::OutputError;

        for transport in [
            || LspError::NotConnected,
            || LspError::Timeout("took too long".to_string()),
        ] {
            let direct = OutputError::from(transport());
            let through_store = OutputError::from(store_error(transport()));
            assert_eq!(
                direct.code, through_store.code,
                "{} is relabelled on the store path",
                direct.message
            );
            assert_eq!(direct.message, through_store.message);
            assert_eq!(direct.hint, through_store.hint);
        }
    }

    /// A page crosses the wire whole: the daemon emits rows, staleness, and
    /// the coverage they were read under, and the client reconstructs the
    /// same `SearchPage` the direct path returns (INV3). Coverage travels
    /// with the rows because a caller that re-read it separately could act
    /// on a different snapshot than the one it is classifying.
    #[test]
    fn a_page_survives_the_wire_round_trip_whole() {
        let emitted = crate::daemon::wire::SearchResponse {
            count: 3,
            items: Vec::<SymbolSearchResult>::new(),
            stale_files: vec!["src/a.rs".to_string()],
            covered: vec!["rust".to_string(), "python".to_string()],
            unread_paths: vec![
                crate::services::store::UnreadPath {
                    path: "src/a".to_string(),
                    is_file: false,
                },
                crate::services::store::UnreadPath {
                    path: "src/b".to_string(),
                    is_file: true,
                },
            ],
        };
        let page: SearchPage<SymbolSearchResult> =
            parse_page(serde_json::to_value(emitted).unwrap()).unwrap();

        assert_eq!(page.total, 3);
        assert!(page.stale());
        assert_eq!(page.covered, vec![Language::Rust, Language::Python]);
        assert_eq!(
            page.unread_paths.len(),
            2,
            "what qualifies the coverage must cross with it, or the daemon path \
             vouches for a completeness the direct path knows it lacks"
        );

        // A fresh, uncovered page carries none of the three qualifiers and
        // reconstructs as empty rather than as absent — they ride on every
        // page otherwise, and they are empty on almost all of them.
        let fresh = crate::daemon::wire::SearchResponse {
            count: 0,
            items: Vec::<SymbolSearchResult>::new(),
            stale_files: Vec::new(),
            covered: Vec::new(),
            unread_paths: Vec::new(),
        };
        let wire = serde_json::to_value(fresh).unwrap();
        for absent in ["stale_files", "covered", "unread_paths"] {
            assert!(wire.get(absent).is_none(), "{absent} rides on every page");
        }
        let page: SearchPage<SymbolSearchResult> = parse_page(wire).unwrap();
        assert!(!page.stale());
        assert!(page.covered.is_empty());
        assert!(page.unread_paths.is_empty());
    }
}
