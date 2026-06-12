use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::IndexingState;
use crate::infra::lsp::protocol::{DocumentSymbol, SymbolInformation};
use crate::models::lsp::{FindSymbolsOptions, Indexed, path_to_uri};
use crate::models::symbol::{Language, Symbol};

use super::converters::*;
use super::helpers::*;
use super::service::{DefaultLspService, ensure_indexed};

pub(super) async fn find_symbols(
    service: &DefaultLspService,
    file: &Path,
    options: FindSymbolsOptions,
) -> Result<Vec<Symbol>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let content = read_file_validated(file, max_file_size).await?;
    let file_path = file.to_path_buf();
    let cache = Arc::clone(&service.symbol_cache);

    let base_options = FindSymbolsOptions {
        include_body: false,
        depth: u32::MAX,
    };

    let cached_symbols = cache
        .get_or_compute(file, &content, || {
            let client_fut = service.get_client_for_file(&file_path);
            let content_clone = content.clone();
            let file_clone = file_path.clone();
            let root = service.manager.root().to_path_buf();
            async move {
                let client = client_fut.await?;
                let uri = path_to_uri(&file_clone);
                client.sync_document(&uri, &content_clone).await?;

                let params = serde_json::json!({
                    "textDocument": { "uri": uri }
                });

                let mut result: serde_json::Value = client
                    .request("textDocument/documentSymbol", Some(params.clone()))
                    .await?;

                // `null` from a server that hasn't reached readiness is a
                // non-answer, not "no symbols". Give the workspace its
                // bounded indexing wait and ask once more before judging.
                if result.is_null() && client.indexing_state() != IndexingState::Ready {
                    ensure_indexed(&client, &file_clone, &root).await;
                    result = client
                        .request("textDocument/documentSymbol", Some(params))
                        .await?;
                }

                parse_document_symbols(
                    result,
                    &file_clone,
                    &base_options,
                    client.indexing_state(),
                    client.language(),
                )
            }
        })
        .await?;

    let mut symbols: Vec<Symbol> = (*cached_symbols).clone();

    if options.include_body {
        apply_body_recursive(&mut symbols, &content);
    }

    if options.depth < u32::MAX {
        symbols = filter_by_depth(symbols, options.depth);
    }

    if symbols.is_empty() {
        symbols.push(create_file_level_symbol(file));
    }

    Ok(symbols)
}

/// Decode a `textDocument/documentSymbol` result. The wire union is
/// `DocumentSymbol[] | SymbolInformation[] | null`, and `null` has two
/// honest readings: from a `Ready` server it means "no symbols"; from one
/// still indexing it means "no answer yet" and must surface as the typed
/// indexing error — an authoritative-looking empty would mislead
/// (invariant 4).
fn parse_document_symbols(
    result: serde_json::Value,
    file: &Path,
    options: &FindSymbolsOptions,
    state: IndexingState,
    language: Language,
) -> Result<Vec<Symbol>, LspError> {
    if result.is_null() {
        return if state == IndexingState::Ready {
            Ok(Vec::new())
        } else {
            Err(LspError::Indexing { language })
        };
    }

    if let Ok(doc_symbols) = serde_json::from_value::<Vec<DocumentSymbol>>(result.clone()) {
        return Ok(convert_document_symbols(
            &doc_symbols,
            file,
            options,
            None,
            None,
            0,
        ));
    }

    let symbols: Vec<SymbolInformation> =
        serde_json::from_value(result).map_err(|e| LspError::Protocol(e.to_string()))?;
    Ok(symbols
        .into_iter()
        .map(|s| {
            let mut sym = Symbol::new(
                s.name,
                convert_symbol_kind(s.kind),
                convert_location(&s.location),
            );
            if let Some(container) = s.container_name
                && !container.is_empty()
            {
                sym = sym.with_container(container);
            }
            sym
        })
        .collect())
}

pub(super) async fn workspace_symbols(
    service: &DefaultLspService,
    query: &str,
    language: Language,
) -> Result<Indexed<Vec<Symbol>>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let manager = Arc::clone(&service.manager);
    let cache = Arc::clone(&service.workspace_symbol_cache);

    // The cache decision must see post-drift state: sweep a live client's
    // overlays first so an external edit bumps the content generation
    // before it is read. Peek only — a cache hit must not boot a server.
    if let Some(client) = manager.peek_client(language).await {
        client.refresh_drifted_overlays().await;
    }
    let generation = crate::infra::lsp::content_generation();

    let symbols = cache
        .get_or_compute(language, query, generation, || {
            let manager = Arc::clone(&manager);
            let query = query.to_string();
            async move {
                let client = manager.get_client(language).await?;

                let root = manager.root();
                if let Some(file) = find_first_file(root, language) {
                    tracing::debug!("Opening file for workspace indexing: {:?}", file);
                    let content = read_file_validated(&file, max_file_size).await?;
                    let uri = path_to_uri(&file);
                    client.sync_document(&uri, &content).await?;
                    client.await_indexing_signal().await;
                } else {
                    tracing::warn!("No {} files found in workspace for indexing", language);
                }
                // Snapshot at computation time: the marker rides with the
                // answer (and gates caching), never a post-hoc state read.
                let ran_under = super::service::degradation_of(client.indexing_state());

                let params = serde_json::json!({ "query": query });
                tracing::debug!(
                    "Sending workspace/symbol request for {} with query: '{}'",
                    language,
                    query
                );

                let symbols: Option<Vec<SymbolInformation>> =
                    client.request("workspace/symbol", Some(params)).await?;

                tracing::debug!(
                    "Received workspace/symbol response: {} symbols",
                    symbols.as_ref().map(|s| s.len()).unwrap_or(0)
                );

                let all_symbols: Vec<Symbol> = symbols
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| {
                        let location = convert_location(&s.location);
                        let mut sym = Symbol::new(s.name, convert_symbol_kind(s.kind), location);
                        if let Some(container) = s.container_name
                            && !container.is_empty()
                        {
                            sym = sym.with_container(container);
                        }
                        sym
                    })
                    .collect();

                Ok(Indexed::new(dedup_symbols(all_symbols), ran_under))
            }
        })
        .await?;

    Ok(Indexed::new((*symbols.data).clone(), symbols.indexing))
}

fn filter_by_depth(symbols: Vec<Symbol>, max_depth: u32) -> Vec<Symbol> {
    fn filter_recursive(symbols: Vec<Symbol>, current_depth: u32, max_depth: u32) -> Vec<Symbol> {
        symbols
            .into_iter()
            .map(|mut sym| {
                if current_depth >= max_depth {
                    sym.children = Vec::new();
                } else if !sym.children.is_empty() {
                    sym.children = filter_recursive(
                        std::mem::take(&mut sym.children),
                        current_depth + 1,
                        max_depth,
                    );
                }
                sym
            })
            .collect()
    }
    filter_recursive(symbols, 0, max_depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse_null(state: IndexingState) -> Result<Vec<Symbol>, LspError> {
        parse_document_symbols(
            serde_json::Value::Null,
            &PathBuf::from("a.rs"),
            &FindSymbolsOptions::default(),
            state,
            Language::Rust,
        )
    }

    #[test]
    fn null_from_a_ready_server_is_an_empty_symbol_list() {
        assert!(parse_null(IndexingState::Ready).unwrap().is_empty());
    }

    /// A `null` before readiness is a non-answer: the typed indexing
    /// error, never an empty list that would read as authoritative.
    #[test]
    fn null_before_readiness_is_the_typed_indexing_error() {
        for state in [
            IndexingState::NotStarted,
            IndexingState::InProgress,
            IndexingState::TimedOut,
        ] {
            match parse_null(state) {
                Err(LspError::Indexing { language }) => {
                    assert_eq!(language, Language::Rust)
                }
                other => panic!("expected the indexing error for {state:?}, got {other:?}"),
            }
        }
    }
}
