//! DefaultLspService implementation

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use super::LspService;
use super::cache::{SymbolCache, WorkspaceSymbolCache};
use super::converters::*;
use super::helpers::*;
use crate::error::LspError;
use crate::infra::lsp::protocol::{
    CallHierarchyIncomingCall, CallHierarchyOutgoingCall, DocumentSymbol, Hover,
    LspCallHierarchyItem, LspDiagnosticSeverity, LspLocation, Position, SymbolInformation,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkspaceEdit,
};
use crate::infra::lsp::{HealthMonitor, LspClient, LspFeature, LspManager};
use crate::models::diagnostic::{Diagnostic, DiagnosticSeverity};
use crate::models::lsp::{
    ApplyActionResult, CallHierarchyItem, CodeAction, CodeActionKind, CodeLens, CodeLensCommand,
    FileChange, FindSymbolsOptions, FoldingRange, FoldingRangeKind, HoverInfo, InlayHint,
    InlayHintKind, PrepareRenameResult, Range, RenameResult, SelectionRange, ServerStatus,
    SignatureHelp, TypeHierarchyItem, path_to_uri, uri_to_path,
};
use crate::models::symbol::{Language, Location, Symbol};

pub struct DefaultLspService {
    manager: Arc<LspManager>,
    symbol_cache: Arc<SymbolCache>,
    workspace_symbol_cache: Arc<WorkspaceSymbolCache>,
    health_shutdown: Arc<AtomicBool>,
}

impl DefaultLspService {
    pub fn new(root: &Path) -> Self {
        let manager = Arc::new(LspManager::new(root.to_path_buf()));
        let monitor = Arc::new(HealthMonitor::new(Arc::clone(&manager)));
        let shutdown = monitor.shutdown_signal();

        tokio::spawn(async move { monitor.run().await });

        Self {
            manager,
            symbol_cache: Arc::new(SymbolCache::default()),
            workspace_symbol_cache: Arc::new(WorkspaceSymbolCache::default()),
            health_shutdown: shutdown,
        }
    }

    pub fn with_manager(manager: Arc<LspManager>) -> Self {
        let monitor = Arc::new(HealthMonitor::new(Arc::clone(&manager)));
        let shutdown = monitor.shutdown_signal();

        tokio::spawn(async move { monitor.run().await });

        Self {
            manager,
            symbol_cache: Arc::new(SymbolCache::default()),
            workspace_symbol_cache: Arc::new(WorkspaceSymbolCache::default()),
            health_shutdown: shutdown,
        }
    }

    fn language_for_file(file: &Path) -> Result<Language, LspError> {
        let language = Language::from_path(file);
        if language == Language::Unknown {
            return Err(LspError::UnsupportedLanguage(
                file.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("unknown")
                    .to_string(),
            ));
        }
        Ok(language)
    }

    async fn get_client_for_file(&self, file: &Path) -> Result<Arc<LspClient>, LspError> {
        let language = Self::language_for_file(file)?;
        self.manager.get_client(language).await
    }

    /// Execute an LSP operation with automatic retry on server termination
    async fn execute_with_retry<F, T, Fut>(&self, file: &Path, op: F) -> Result<T, LspError>
    where
        F: Fn(Arc<LspClient>) -> Fut,
        Fut: std::future::Future<Output = Result<T, LspError>>,
    {
        let language = Self::language_for_file(file)?;
        self.manager.execute_with_retry(language, op).await
    }

    async fn ensure_workspace_indexed(&self, client: &LspClient, file: &Path) {
        use crate::infra::lsp::client::IndexingState;

        let state = client.indexing_state();
        if state == IndexingState::Ready {
            return;
        }

        if matches!(state, IndexingState::NotStarted | IndexingState::Stale) {
            let language = Language::from_path(file);
            if let Some(entry_file) = find_project_entry(self.manager.root(), language)
                && let Ok(content) = tokio::fs::read_to_string(&entry_file).await
            {
                let _ = client
                    .sync_document(&path_to_uri(&entry_file), &content)
                    .await;
            }
        }

        client.wait_for_indexing().await;
    }

    async fn sync_document(&self, client: &LspClient, file: &Path) -> Result<String, LspError> {
        let content = read_file_validated(file).await?;
        let uri = path_to_uri(file);
        client.sync_document(&uri, &content).await?;
        Ok(uri)
    }

    /// Unified preparation for position-based LSP requests.
    /// Ensures workspace is indexed and document is synced before making the request.
    async fn prepare_for_request(&self, file: &Path) -> Result<(Arc<LspClient>, String), LspError> {
        let client = self.get_client_for_file(file).await?;
        self.ensure_workspace_indexed(&client, file).await;
        let uri = self.sync_document(&client, file).await?;
        Ok((client, uri))
    }

    async fn prepare_for_cross_file_request(
        &self,
        file: &Path,
    ) -> Result<(Arc<LspClient>, String), LspError> {
        let client = self.get_client_for_file(file).await?;
        self.ensure_workspace_indexed(&client, file).await;
        client.ensure_cross_file_ready().await;
        let uri = self.sync_document(&client, file).await?;
        Ok((client, uri))
    }

    async fn wait_for_diagnostics(
        client: &LspClient,
        uri: &str,
    ) -> Vec<crate::infra::lsp::protocol::LspDiagnostic> {
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);
        const MAX_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

        let start = std::time::Instant::now();
        while start.elapsed() < MAX_WAIT {
            let diags = client.get_diagnostics(uri).await;
            if !diags.is_empty() {
                return diags;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        client.get_diagnostics(uri).await
    }

    fn filter_by_depth(symbols: Vec<Symbol>, max_depth: u32) -> Vec<Symbol> {
        fn filter_recursive(
            symbols: Vec<Symbol>,
            current_depth: u32,
            max_depth: u32,
        ) -> Vec<Symbol> {
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
}

#[async_trait]
impl LspService for DefaultLspService {
    async fn find_symbols(
        &self,
        file: &Path,
        options: FindSymbolsOptions,
    ) -> Result<Vec<Symbol>, LspError> {
        let content = read_file_validated(file).await?;
        let file_path = file.to_path_buf();
        let cache = Arc::clone(&self.symbol_cache);

        // Use cache for base symbol lookup (full depth, no body)
        let base_options = FindSymbolsOptions {
            include_body: false,
            depth: u32::MAX,
        };

        let cached_symbols = cache
            .get_or_compute(file, &content, || {
                let client_fut = self.get_client_for_file(&file_path);
                let content_clone = content.clone();
                let file_clone = file_path.clone();
                async move {
                    let client = client_fut.await?;
                    let uri = path_to_uri(&file_clone);
                    client.sync_document(&uri, &content_clone).await?;

                    let params = serde_json::json!({
                        "textDocument": { "uri": uri }
                    });

                    // Try hierarchical document symbols first
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

                    // Fallback to flat symbols
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

        // Apply options to cached symbols
        let mut symbols: Vec<Symbol> = (*cached_symbols).clone();

        // Add body if requested
        if options.include_body {
            for sym in &mut symbols {
                if let Some(body) = extract_body(&content, &sym.location) {
                    *sym = sym.clone().with_body(body);
                }
            }
        }

        // Apply depth filtering
        if options.depth < u32::MAX {
            symbols = Self::filter_by_depth(symbols, options.depth);
        }

        // Graceful degradation: return file-level symbol if no symbols found
        if symbols.is_empty() {
            symbols.push(create_file_level_symbol(file));
        }

        Ok(symbols)
    }

    async fn workspace_symbols(
        &self,
        query: &str,
        language: Language,
    ) -> Result<Vec<Symbol>, LspError> {
        let manager = Arc::clone(&self.manager);
        let cache = Arc::clone(&self.workspace_symbol_cache);

        let symbols = cache
            .get_or_compute(language, query, || {
                let manager = Arc::clone(&manager);
                let query = query.to_string();
                async move {
                    let client = manager.get_client(language).await?;

                    let root = manager.root();
                    if let Some(file) = find_first_file(root, language) {
                        tracing::debug!("Opening file for workspace indexing: {:?}", file);
                        let content = read_file_validated(&file).await?;
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

                    let mut seen = std::collections::HashSet::new();

                    Ok(symbols
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|s| {
                            let location = convert_location(&s.location);
                            let key = (
                                s.name.clone(),
                                s.kind as u32,
                                location.file.clone(),
                                location.line,
                                location.column,
                            );

                            if seen.insert(key) {
                                let mut sym =
                                    Symbol::new(s.name, convert_symbol_kind(s.kind), location);
                                if let Some(container) = s.container_name
                                    && !container.is_empty()
                                {
                                    sym = sym.with_container(container);
                                }
                                Some(sym)
                            } else {
                                None
                            }
                        })
                        .collect())
                }
            })
            .await?;

        Ok((*symbols).clone())
    }

    async fn find_references(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError> {
        let file = file.to_path_buf();
        let manager = Arc::clone(&self.manager);
        let project_root = manager.root().to_path_buf();

        self.execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            let project_root = project_root.clone();
            async move {
                use crate::infra::lsp::client::IndexingState;
                let state = client.indexing_state();
                if state != IndexingState::Ready {
                    if matches!(state, IndexingState::NotStarted | IndexingState::Stale) {
                        let language = Language::from_path(&file);
                        if let Some(entry_file) = find_project_entry(manager.root(), language)
                            && let Ok(content) = tokio::fs::read_to_string(&entry_file).await
                        {
                            let _ = client
                                .sync_document(&path_to_uri(&entry_file), &content)
                                .await;
                        }
                    }
                    client.wait_for_indexing().await;
                }

                client.ensure_cross_file_ready().await;

                let content = read_file_validated(&file).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column),
                    "context": { "includeDeclaration": true }
                });

                let result: serde_json::Value = client
                    .request("textDocument/references", Some(params))
                    .await?;

                if result.is_null() {
                    return Ok(Vec::new());
                }

                let locations: Vec<LspLocation> = serde_json::from_value(result)
                    .map_err(|e| LspError::Protocol(e.to_string()))?;

                let all_locations: Vec<Location> = locations.iter().map(convert_location).collect();

                Ok(filter_locations_within_project(
                    all_locations,
                    &project_root,
                ))
            }
        })
        .await
    }

    async fn goto_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Location>, LspError> {
        let (client, uri) = self.prepare_for_cross_file_request(file).await?;
        let language = Language::from_path(file);

        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(&uri),
            position: to_lsp_position(line, column),
        };

        let result: serde_json::Value = client
            .request(
                "textDocument/definition",
                Some(serde_json::to_value(params)?),
            )
            .await?;

        Ok(parse_location_response(&result)
            .and_then(|locs| select_best_definition(&locs, language).map(convert_location)))
    }

    async fn goto_type_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Location>, LspError> {
        check_feature_support(file, LspFeature::GotoTypeDefinition)?;

        let (client, uri) = self.prepare_for_cross_file_request(file).await?;

        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(&uri),
            position: to_lsp_position(line, column),
        };

        let result: serde_json::Value = client
            .request(
                "textDocument/typeDefinition",
                Some(serde_json::to_value(params)?),
            )
            .await?;

        Ok(parse_location_response(&result).and_then(|locs| locs.first().map(convert_location)))
    }

    async fn find_implementations(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError> {
        check_feature_support(file, LspFeature::FindImplementations)?;

        let (client, uri) = self.prepare_for_cross_file_request(file).await?;

        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(&uri),
            position: to_lsp_position(line, column),
        };

        let result: serde_json::Value = client
            .request(
                "textDocument/implementation",
                Some(serde_json::to_value(params)?),
            )
            .await?;

        Ok(parse_location_response(&result)
            .map(|locs| locs.iter().map(convert_location).collect())
            .unwrap_or_default())
    }

    async fn hover(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<HoverInfo>, LspError> {
        let file = file.to_path_buf();
        let manager = Arc::clone(&self.manager);

        self.execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                use crate::infra::lsp::client::IndexingState;

                // Ensure workspace is indexed
                let state = client.indexing_state();
                if state != IndexingState::Ready {
                    if matches!(state, IndexingState::NotStarted | IndexingState::Stale) {
                        let language = Language::from_path(&file);
                        if let Some(entry_file) = find_project_entry(manager.root(), language)
                            && let Ok(content) = tokio::fs::read_to_string(&entry_file).await
                        {
                            let _ = client
                                .sync_document(&path_to_uri(&entry_file), &content)
                                .await;
                        }
                    }
                    client.wait_for_indexing().await;
                }

                // Sync document
                let content = read_file_validated(&file).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(line, column),
                };

                let result: Option<Hover> = client
                    .request("textDocument/hover", Some(serde_json::to_value(params)?))
                    .await?;

                Ok(result.map(|h| {
                    let content = extract_hover_content(&h.contents);
                    let range = h.range.map(|r| range_to_location(&file, &r));
                    HoverInfo { content, range }
                }))
            }
        })
        .await
    }

    async fn signature_help(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<SignatureHelp>, LspError> {
        let (client, uri) = self.prepare_for_request(file).await?;

        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(&uri),
            position: to_lsp_position(line, column),
        };

        let result: Option<serde_json::Value> = client
            .request(
                "textDocument/signatureHelp",
                Some(serde_json::to_value(params)?),
            )
            .await?;

        Ok(result.and_then(|v| parse_signature_help(&v)))
    }

    async fn diagnostics(&self, file: &Path) -> Result<Vec<Diagnostic>, LspError> {
        use crate::infra::lsp::protocol::LspDiagnosticTag;
        use crate::models::diagnostic::DiagnosticTag;

        let client = self.get_client_for_file(file).await?;
        let uri = self.sync_document(&client, file).await?;

        let lsp_diagnostics = Self::wait_for_diagnostics(&client, &uri).await;

        let diagnostics = lsp_diagnostics
            .into_iter()
            .map(|d| {
                use crate::models::lsp::{Position as LspPosition, Range as LspRange};

                let severity = match d.severity {
                    Some(s) => match s {
                        LspDiagnosticSeverity::Error => DiagnosticSeverity::Error,
                        LspDiagnosticSeverity::Warning => DiagnosticSeverity::Warning,
                        LspDiagnosticSeverity::Information => DiagnosticSeverity::Information,
                        LspDiagnosticSeverity::Hint => DiagnosticSeverity::Hint,
                    },
                    None => DiagnosticSeverity::Error,
                };

                let tags: Vec<DiagnosticTag> = d
                    .tags
                    .iter()
                    .map(|t| match t {
                        LspDiagnosticTag::Unnecessary => DiagnosticTag::Unnecessary,
                        LspDiagnosticTag::Deprecated => DiagnosticTag::Deprecated,
                    })
                    .collect();

                let related_information = d
                    .related_information
                    .into_iter()
                    .map(|r| crate::models::diagnostic::DiagnosticRelatedInfo {
                        location: convert_location(&r.location),
                        message: r.message,
                    })
                    .collect();

                Diagnostic {
                    file_path: file.display().to_string(),
                    range: LspRange {
                        start: LspPosition {
                            line: d.range.start.line,
                            character: d.range.start.character,
                        },
                        end: LspPosition {
                            line: d.range.end.line,
                            character: d.range.end.character,
                        },
                    },
                    severity,
                    message: d.message,
                    code: d.code.map(|c| c.to_string()),
                    source: d.source,
                    tags,
                    related_information,
                }
            })
            .collect();

        Ok(diagnostics)
    }

    async fn prepare_rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<PrepareRenameResult>, LspError> {
        let client = self.get_client_for_file(file).await?;
        let uri = self.sync_document(&client, file).await?;

        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": to_lsp_position(line, column)
        });

        // Use proper error handling - only certain errors indicate "not renameable"
        let result: Result<Option<serde_json::Value>, _> = client
            .request("textDocument/prepareRename", Some(params))
            .await;

        let value = match result {
            Ok(Some(v)) if !v.is_null() => v,
            Ok(_) => return Ok(None), // null or None means not renameable
            Err(LspError::Protocol(msg)) if msg.contains("cannot be renamed") => return Ok(None),
            Err(e) => return Err(e),
        };

        // Helper to extract position from JSON
        fn extract_position(pos: &serde_json::Value) -> Option<crate::models::lsp::Position> {
            Some(crate::models::lsp::Position {
                line: pos.get("line")?.as_u64()? as u32,
                character: pos.get("character")?.as_u64()? as u32,
            })
        }

        // Format 1: { placeholder: string, range: Range }
        if let Some(placeholder) = value.get("placeholder").and_then(|p| p.as_str())
            && let Some(range) = value.get("range")
        {
            let start = range.get("start").and_then(extract_position);
            let end = range.get("end").and_then(extract_position);
            if let (Some(start), Some(end)) = (start, end) {
                return Ok(Some(PrepareRenameResult {
                    placeholder: placeholder.to_string(),
                    range: crate::models::lsp::Range { start, end },
                }));
            }
        }

        // Format 2: Range (just start/end positions, extract placeholder from source)
        if let (Some(start), Some(end)) = (value.get("start"), value.get("end")) {
            let start_pos = extract_position(start);
            let end_pos = extract_position(end);
            if let (Some(start_pos), Some(end_pos)) = (start_pos, end_pos) {
                // Extract placeholder from source file
                let placeholder =
                    read_line_streaming(file, start_pos.line)
                        .await
                        .and_then(|line| {
                            let s = start_pos.character as usize;
                            let e = end_pos.character as usize;
                            if s < line.len() && e <= line.len() && s < e {
                                Some(line[s..e].to_string())
                            } else {
                                None
                            }
                        });

                if let Some(placeholder) = placeholder {
                    return Ok(Some(PrepareRenameResult {
                        placeholder,
                        range: crate::models::lsp::Range {
                            start: start_pos,
                            end: end_pos,
                        },
                    }));
                }
            }
        }

        // Format 3: { defaultBehavior: true } - symbol can be renamed, but no range provided
        if value.get("defaultBehavior").and_then(|v| v.as_bool()) == Some(true) {
            // Fallback: use the original position
            return Ok(Some(PrepareRenameResult {
                placeholder: String::new(), // Client should prompt for name
                range: crate::models::lsp::Range {
                    start: crate::models::lsp::Position {
                        line: line.saturating_sub(1),
                        character: column.saturating_sub(1),
                    },
                    end: crate::models::lsp::Position {
                        line: line.saturating_sub(1),
                        character: column,
                    },
                },
            }));
        }

        Ok(None)
    }

    async fn rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
        new_name: &str,
    ) -> Result<RenameResult, LspError> {
        let content = read_file_validated(file).await?;
        let uri = path_to_uri(file);
        let new_name = new_name.to_string();

        let result: serde_json::Value = self
            .execute_with_retry(file, |client| {
                let uri = uri.clone();
                let content = content.clone();
                let new_name = new_name.clone();
                async move {
                    client.sync_document(&uri, &content).await?;
                    let params = serde_json::json!({
                        "textDocument": { "uri": uri },
                        "position": to_lsp_position(line, column),
                        "newName": new_name
                    });
                    client.request("textDocument/rename", Some(params)).await
                }
            })
            .await?;

        if result.is_null() {
            return Err(LspError::Protocol(
                "Symbol at this position cannot be renamed. Try a different position or symbol."
                    .to_string(),
            ));
        }

        let edit: WorkspaceEdit = serde_json::from_value(result)
            .map_err(|e| LspError::Protocol(format!("Invalid rename response: {}", e)))?;

        let changes = if let Some(changes_map) = edit.changes {
            changes_map
                .into_iter()
                .map(|(uri, edits)| FileChange {
                    file: uri_to_path(&uri),
                    edit_count: edits.len(),
                })
                .collect()
        } else if let Some(doc_changes) = edit.document_changes {
            doc_changes
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let text_doc = item.get("textDocument")?;
                            let uri = text_doc.get("uri")?.as_str()?;
                            let edits = item.get("edits")?.as_array()?;
                            Some(FileChange {
                                file: uri_to_path(uri),
                                edit_count: edits.len(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(RenameResult { changes })
    }

    async fn incoming_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError> {
        check_feature_support(file, LspFeature::IncomingCalls)?;

        let (client, uri) = self.prepare_for_cross_file_request(file).await?;

        let prepare_params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": to_lsp_position(line, column)
        });

        let items: Option<Vec<LspCallHierarchyItem>> = client
            .request("textDocument/prepareCallHierarchy", Some(prepare_params))
            .await?;

        let items = items.unwrap_or_default();
        if items.is_empty() {
            return Ok(vec![]);
        }

        let incoming_params = serde_json::json!({ "item": items[0] });

        let incoming: Option<Vec<CallHierarchyIncomingCall>> = client
            .request("callHierarchy/incomingCalls", Some(incoming_params))
            .await?;

        Ok(incoming
            .unwrap_or_default()
            .into_iter()
            .map(|c| CallHierarchyItem {
                name: c.from.name,
                kind: convert_symbol_kind(c.from.kind),
                location: uri_range_to_location(&c.from.uri, &c.from.selection_range),
                call_site: c
                    .from_ranges
                    .first()
                    .map(|r| uri_range_to_location(&c.from.uri, r)),
            })
            .collect())
    }

    async fn outgoing_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError> {
        check_feature_support(file, LspFeature::OutgoingCalls)?;

        let (client, uri) = self.prepare_for_cross_file_request(file).await?;

        let prepare_params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": to_lsp_position(line, column)
        });

        let items: Option<Vec<LspCallHierarchyItem>> = client
            .request("textDocument/prepareCallHierarchy", Some(prepare_params))
            .await?;

        let items = items.unwrap_or_default();
        if items.is_empty() {
            return Ok(vec![]);
        }

        let outgoing_params = serde_json::json!({ "item": items[0] });

        let outgoing: Option<Vec<CallHierarchyOutgoingCall>> = client
            .request("callHierarchy/outgoingCalls", Some(outgoing_params))
            .await?;

        Ok(outgoing
            .unwrap_or_default()
            .into_iter()
            .map(|c| CallHierarchyItem {
                name: c.to.name,
                kind: convert_symbol_kind(c.to.kind),
                location: uri_range_to_location(&c.to.uri, &c.to.selection_range),
                call_site: c
                    .from_ranges
                    .first()
                    .map(|r| uri_range_to_location(&c.to.uri, r)),
            })
            .collect())
    }

    async fn supertypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<TypeHierarchyItem>, LspError> {
        check_feature_support(file, LspFeature::TypeHierarchy)?;

        let (client, uri) = self.prepare_for_request(file).await?;

        let prepare_params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": to_lsp_position(line, column)
        });

        let items: Option<Vec<serde_json::Value>> = client
            .request("textDocument/prepareTypeHierarchy", Some(prepare_params))
            .await?;

        let items = items.unwrap_or_default();
        if items.is_empty() {
            return Ok(vec![]);
        }

        let supertypes_params = serde_json::json!({ "item": items[0] });

        let supertypes: Option<Vec<serde_json::Value>> = client
            .request("typeHierarchy/supertypes", Some(supertypes_params))
            .await?;

        Ok(supertypes
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| parse_type_hierarchy_item(&item))
            .collect())
    }

    async fn subtypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<TypeHierarchyItem>, LspError> {
        check_feature_support(file, LspFeature::TypeHierarchy)?;

        let (client, uri) = self.prepare_for_request(file).await?;

        let prepare_params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": to_lsp_position(line, column)
        });

        let items: Option<Vec<serde_json::Value>> = client
            .request("textDocument/prepareTypeHierarchy", Some(prepare_params))
            .await?;

        let items = items.unwrap_or_default();
        if items.is_empty() {
            return Ok(vec![]);
        }

        let subtypes_params = serde_json::json!({ "item": items[0] });

        let subtypes: Option<Vec<serde_json::Value>> = client
            .request("typeHierarchy/subtypes", Some(subtypes_params))
            .await?;

        Ok(subtypes
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| parse_type_hierarchy_item(&item))
            .collect())
    }

    async fn inlay_hints(&self, file: &Path, range: Range) -> Result<Vec<InlayHint>, LspError> {
        check_feature_support(file, LspFeature::InlayHints)?;

        let client = self.get_client_for_file(file).await?;
        let uri = self.sync_document(&client, file).await?;

        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": range.start.line, "character": range.start.character },
                "end": { "line": range.end.line, "character": range.end.character }
            }
        });

        let hints: Option<Vec<serde_json::Value>> = client
            .request("textDocument/inlayHint", Some(params))
            .await?;

        Ok(hints
            .unwrap_or_default()
            .into_iter()
            .filter_map(|h| {
                let position = h.get("position")?;
                let pos = crate::models::lsp::Position::new(
                    position.get("line")?.as_u64()? as u32,
                    position.get("character")?.as_u64()? as u32,
                );

                let label = match h.get("label")? {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(arr) => arr
                        .iter()
                        .filter_map(|part| part.get("value").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join(""),
                    _ => return None,
                };

                let kind = InlayHintKind::from_lsp(
                    h.get("kind").and_then(|k| k.as_u64()).map(|k| k as u32),
                );
                let padding_left = h
                    .get("paddingLeft")
                    .and_then(|p| p.as_bool())
                    .unwrap_or(false);
                let padding_right = h
                    .get("paddingRight")
                    .and_then(|p| p.as_bool())
                    .unwrap_or(false);

                Some(InlayHint {
                    position: pos,
                    label,
                    kind,
                    padding_left,
                    padding_right,
                })
            })
            .collect())
    }

    async fn folding_ranges(&self, file: &Path) -> Result<Vec<FoldingRange>, LspError> {
        let client = self.get_client_for_file(file).await?;
        let uri = self.sync_document(&client, file).await?;

        let params = serde_json::json!({
            "textDocument": { "uri": uri }
        });

        let ranges: Option<Vec<serde_json::Value>> = client
            .request("textDocument/foldingRange", Some(params))
            .await?;

        Ok(ranges
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                let start_line = r.get("startLine")?.as_u64()? as u32;
                let end_line = r.get("endLine")?.as_u64()? as u32;
                let start_character = r
                    .get("startCharacter")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let end_character = r
                    .get("endCharacter")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let kind = FoldingRangeKind::from_lsp(r.get("kind").and_then(|k| k.as_str()));
                let collapsed_text = r
                    .get("collapsedText")
                    .and_then(|t| t.as_str())
                    .map(String::from);

                Some(FoldingRange {
                    start_line,
                    end_line,
                    start_character,
                    end_character,
                    kind,
                    collapsed_text,
                })
            })
            .collect())
    }

    async fn selection_ranges(
        &self,
        file: &Path,
        positions: Vec<(u32, u32)>,
    ) -> Result<Vec<SelectionRange>, LspError> {
        let client = self.get_client_for_file(file).await?;
        let uri = self.sync_document(&client, file).await?;

        let lsp_positions: Vec<_> = positions
            .iter()
            .map(|(line, col)| to_lsp_position(*line, *col))
            .collect();

        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "positions": lsp_positions
        });

        let ranges: Option<Vec<serde_json::Value>> = client
            .request("textDocument/selectionRange", Some(params))
            .await?;

        fn parse_selection_range(value: &serde_json::Value) -> Option<SelectionRange> {
            let range = value.get("range")?;
            let start = range.get("start")?;
            let end = range.get("end")?;

            let start_pos = crate::models::lsp::Position::new(
                start.get("line")?.as_u64()? as u32,
                start.get("character")?.as_u64()? as u32,
            );
            let end_pos = crate::models::lsp::Position::new(
                end.get("line")?.as_u64()? as u32,
                end.get("character")?.as_u64()? as u32,
            );

            let parent = value
                .get("parent")
                .and_then(|p| parse_selection_range(p).map(Box::new));

            Some(SelectionRange {
                range: Range::new(start_pos, end_pos),
                parent,
            })
        }

        Ok(ranges
            .unwrap_or_default()
            .iter()
            .filter_map(parse_selection_range)
            .collect())
    }

    async fn code_lens(&self, file: &Path) -> Result<Vec<CodeLens>, LspError> {
        let client = self.get_client_for_file(file).await?;
        let uri = self.sync_document(&client, file).await?;

        let params = serde_json::json!({
            "textDocument": { "uri": uri }
        });

        let lenses: Option<Vec<serde_json::Value>> = client
            .request("textDocument/codeLens", Some(params))
            .await?;

        Ok(lenses
            .unwrap_or_default()
            .into_iter()
            .filter_map(|lens| {
                let range = lens.get("range")?;
                let start = range.get("start")?;
                let end = range.get("end")?;

                let start_pos = crate::models::lsp::Position::new(
                    start.get("line")?.as_u64()? as u32,
                    start.get("character")?.as_u64()? as u32,
                );
                let end_pos = crate::models::lsp::Position::new(
                    end.get("line")?.as_u64()? as u32,
                    end.get("character")?.as_u64()? as u32,
                );

                let command = lens.get("command").and_then(|cmd| {
                    Some(CodeLensCommand {
                        title: cmd.get("title")?.as_str()?.to_string(),
                        command: cmd.get("command")?.as_str()?.to_string(),
                        arguments: cmd
                            .get("arguments")
                            .and_then(|a| a.as_array())
                            .cloned()
                            .unwrap_or_default(),
                    })
                });

                let data = lens.get("data").cloned();

                Some(CodeLens {
                    range: Range::new(start_pos, end_pos),
                    command,
                    data,
                })
            })
            .collect())
    }

    async fn code_actions(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CodeAction>, LspError> {
        check_feature_support(file, LspFeature::CodeActions)?;

        let (client, uri) = self.prepare_for_request(file).await?;

        let position = Position::new(line.saturating_sub(1), column.saturating_sub(1));

        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": position.line, "character": position.character },
                "end": { "line": position.line, "character": position.character + 1 }
            },
            "context": { "diagnostics": [] }
        });

        let response: Option<Vec<serde_json::Value>> = client
            .request("textDocument/codeAction", Some(params))
            .await?;

        let actions = response
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| {
                let title = v.get("title")?.as_str()?.to_string();
                let kind = v.get("kind").and_then(|k| k.as_str());
                let is_preferred = v
                    .get("isPreferred")
                    .and_then(|p| p.as_bool())
                    .unwrap_or(false);
                let diagnostics = v
                    .get("diagnostics")
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default();

                Some(CodeAction {
                    title,
                    kind: CodeActionKind::from(kind),
                    is_preferred,
                    diagnostics,
                    edit: None,
                    data: Some(v),
                })
            })
            .collect();

        Ok(actions)
    }

    async fn apply_code_action(
        &self,
        file: &Path,
        action: &CodeAction,
    ) -> Result<ApplyActionResult, LspError> {
        let client = self.get_client_for_file(file).await?;
        let _ = self.sync_document(&client, file).await?;

        let raw_data = action.data.as_ref().unwrap_or(&serde_json::Value::Null);
        let edit = raw_data.get("edit").cloned();

        let edit = if edit.is_none() && action.data.is_some() {
            let resolved: Option<serde_json::Value> = client
                .request("codeAction/resolve", action.data.clone())
                .await
                .ok()
                .flatten();

            resolved.and_then(|r| r.get("edit").cloned())
        } else {
            edit
        };

        let changes = if let Some(edit) = edit {
            parse_workspace_edit(&edit)
        } else {
            Vec::new()
        };

        Ok(ApplyActionResult { changes })
    }

    async fn is_available(&self, language: Language) -> bool {
        self.manager.is_available(language)
    }

    async fn server_status(&self, language: Language) -> ServerStatus {
        self.manager.server_status(language).await.into()
    }

    async fn shutdown(&self) {
        self.health_shutdown.store(true, Ordering::Release);
        self.manager.shutdown_all().await;
    }

    async fn cleanup_idle(&self, timeout: std::time::Duration) -> usize {
        self.manager.cleanup_idle(timeout).await
    }
}
