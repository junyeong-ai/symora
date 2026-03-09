use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::protocol::{DocumentSymbol, SymbolInformation};
use crate::models::lsp::{FindSymbolsOptions, path_to_uri};
use crate::models::symbol::{Language, Symbol};

use super::converters::*;
use super::helpers::*;
use super::service::DefaultLspService;

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
            async move {
                let client = client_fut.await?;
                let uri = path_to_uri(&file_clone);
                client.sync_document(&uri, &content_clone).await?;

                let params = serde_json::json!({
                    "textDocument": { "uri": uri }
                });

                let result: Result<Vec<DocumentSymbol>, _> = client
                    .request("textDocument/documentSymbol", Some(params.clone()))
                    .await;

                if let Ok(doc_symbols) = result {
                    return Ok(convert_document_symbols(
                        &doc_symbols,
                        &file_clone,
                        &base_options,
                        None,
                        None,
                        0,
                    ));
                }

                let symbols: Vec<SymbolInformation> = client
                    .request("textDocument/documentSymbol", Some(params))
                    .await?;

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

pub(super) async fn workspace_symbols(
    service: &DefaultLspService,
    query: &str,
    language: Language,
) -> Result<Vec<Symbol>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let manager = Arc::clone(&service.manager);
    let cache = Arc::clone(&service.workspace_symbol_cache);

    let symbols = cache
        .get_or_compute(language, query, || {
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
                    client.wait_for_indexing().await;
                } else {
                    tracing::warn!("No {} files found in workspace for indexing", language);
                }

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

                Ok(dedup_symbols(all_symbols))
            }
        })
        .await?;

    Ok((*symbols).clone())
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
