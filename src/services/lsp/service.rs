use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use super::LspService;
use super::cache::{SymbolCache, WorkspaceSymbolCache};
use crate::error::LspError;
use crate::infra::lsp::{HealthMonitor, IndexingState, LspClient, LspManager};
use crate::models::diagnostic::Diagnostic;
use crate::models::lsp::{
    ApplyActionResult, CallHierarchyItem, CodeAction, CodeLens, FindSymbolsOptions, FoldingRange,
    HoverInfo, IndexingDegradation, InlayHint, PrepareRenameResult, Range, RenameResult,
    SelectionRange, ServerStatus, SignatureHelp, TextEdit, TypeHierarchyItem, path_to_uri,
};
use crate::models::symbol::{Language, Location, Symbol};

use super::{editor, hierarchy, info, lifecycle, navigation, rename, symbols};

pub struct DefaultLspService {
    pub(super) manager: Arc<LspManager>,
    pub(super) symbol_cache: Arc<SymbolCache>,
    pub(super) workspace_symbol_cache: Arc<WorkspaceSymbolCache>,
    pub(super) health_shutdown: Arc<AtomicBool>,
    pub(super) health_handle: tokio::task::JoinHandle<()>,
}

impl DefaultLspService {
    pub fn new(root: &Path, config: Arc<crate::config::LspRuntimeConfig>) -> Self {
        Self::init_with_manager(Arc::new(LspManager::new(root.to_path_buf(), config)))
    }

    fn init_with_manager(manager: Arc<LspManager>) -> Self {
        let monitor = Arc::new(HealthMonitor::new(Arc::clone(&manager)));
        let shutdown = monitor.shutdown_signal();
        let handle = tokio::spawn(async move { monitor.run().await });

        Self {
            manager,
            symbol_cache: Arc::new(SymbolCache::default()),
            workspace_symbol_cache: Arc::new(WorkspaceSymbolCache::default()),
            health_shutdown: shutdown,
            health_handle: handle,
        }
    }

    pub(super) fn max_file_size_bytes(&self) -> u64 {
        self.manager.runtime_config().max_file_size_bytes
    }

    pub(super) fn language_for_file(file: &Path) -> Result<Language, LspError> {
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

    pub(super) async fn get_client_for_file(
        &self,
        file: &Path,
    ) -> Result<Arc<LspClient>, LspError> {
        let language = Self::language_for_file(file)?;
        self.manager.get_client(language).await
    }

    pub(super) async fn execute_with_retry<F, T, Fut>(
        &self,
        file: &Path,
        op: F,
    ) -> Result<T, LspError>
    where
        F: Fn(Arc<LspClient>) -> Fut,
        Fut: std::future::Future<Output = Result<T, LspError>>,
    {
        let language = Self::language_for_file(file)?;
        let ws_cache = Arc::clone(&self.workspace_symbol_cache);

        self.manager
            .execute_with_retry(language, |client| {
                let ws_cache = Arc::clone(&ws_cache);
                let fut = op(Arc::clone(&client));
                async move {
                    match fut.await {
                        Ok(result) => Ok(result),
                        Err(e) if e.needs_restart() => {
                            ws_cache.invalidate_language(language).await;
                            Err(e)
                        }
                        Err(e) => Err(e),
                    }
                }
            })
            .await
    }
}

#[async_trait]
impl LspService for DefaultLspService {
    async fn find_symbols(
        &self,
        file: &Path,
        options: FindSymbolsOptions,
    ) -> Result<Vec<Symbol>, LspError> {
        symbols::find_symbols(self, file, options).await
    }

    async fn workspace_symbols(
        &self,
        query: &str,
        language: Language,
    ) -> Result<Vec<Symbol>, LspError> {
        symbols::workspace_symbols(self, query, language).await
    }

    async fn find_references(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError> {
        navigation::find_references(self, file, line, column).await
    }

    async fn goto_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Location>, LspError> {
        navigation::goto_definition(self, file, line, column).await
    }

    async fn goto_type_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Location>, LspError> {
        navigation::goto_type_definition(self, file, line, column).await
    }

    async fn find_implementations(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError> {
        navigation::find_implementations(self, file, line, column).await
    }

    async fn hover(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<HoverInfo>, LspError> {
        info::hover(self, file, line, column).await
    }

    async fn signature_help(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<SignatureHelp>, LspError> {
        info::signature_help(self, file, line, column).await
    }

    async fn diagnostics(&self, file: &Path) -> Result<Vec<Diagnostic>, LspError> {
        info::diagnostics(self, file).await
    }

    async fn prepare_rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<PrepareRenameResult>, LspError> {
        rename::prepare_rename(self, file, line, column).await
    }

    async fn rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
        new_name: &str,
    ) -> Result<RenameResult, LspError> {
        rename::rename(self, file, line, column, new_name).await
    }

    async fn incoming_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError> {
        hierarchy::incoming_calls(self, file, line, column).await
    }

    async fn outgoing_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError> {
        hierarchy::outgoing_calls(self, file, line, column).await
    }

    async fn supertypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<TypeHierarchyItem>, LspError> {
        hierarchy::supertypes(self, file, line, column).await
    }

    async fn subtypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<TypeHierarchyItem>, LspError> {
        hierarchy::subtypes(self, file, line, column).await
    }

    async fn inlay_hints(&self, file: &Path, range: Range) -> Result<Vec<InlayHint>, LspError> {
        editor::inlay_hints(self, file, range).await
    }

    async fn folding_ranges(&self, file: &Path) -> Result<Vec<FoldingRange>, LspError> {
        editor::folding_ranges(self, file).await
    }

    async fn selection_ranges(
        &self,
        file: &Path,
        positions: Vec<(u32, u32)>,
    ) -> Result<Vec<SelectionRange>, LspError> {
        editor::selection_ranges(self, file, positions).await
    }

    async fn code_lenses(&self, file: &Path) -> Result<Vec<CodeLens>, LspError> {
        editor::code_lenses(self, file).await
    }

    async fn code_actions(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CodeAction>, LspError> {
        editor::code_actions(self, file, line, column).await
    }

    async fn apply_code_action(
        &self,
        file: &Path,
        action: &CodeAction,
    ) -> Result<ApplyActionResult, LspError> {
        editor::apply_code_action(self, file, action).await
    }

    async fn format(&self, file: &Path) -> Result<Vec<TextEdit>, LspError> {
        editor::format(self, file).await
    }

    async fn is_available(&self, language: Language) -> bool {
        lifecycle::is_available(self, language).await
    }

    async fn server_status(&self, language: Language) -> ServerStatus {
        lifecycle::server_status(self, language).await
    }

    async fn indexing_degradation(&self, language: Language) -> Option<IndexingDegradation> {
        let client = self.manager.peek_client(language).await?;
        match client.indexing_state() {
            IndexingState::TimedOut => Some(IndexingDegradation::TimedOut),
            _ => None,
        }
    }
}

impl Drop for DefaultLspService {
    fn drop(&mut self) {
        self.health_shutdown.store(true, Ordering::Release);
        self.health_handle.abort();
    }
}

pub(super) async fn ensure_indexed(client: &LspClient, file: &Path, root: &Path) {
    use super::helpers::find_project_entry;
    use crate::infra::lsp::client::IndexingState;

    let state = client.indexing_state();
    if state.is_usable() {
        return;
    }

    if matches!(state, IndexingState::NotStarted | IndexingState::Stale) {
        let language = Language::from_path(file);
        if let Some(entry_file) = find_project_entry(root, language, client.config())
            && let Ok(content) = tokio::fs::read_to_string(&entry_file).await
            && let Err(e) = client
                .sync_document(&path_to_uri(&entry_file), &content)
                .await
        {
            tracing::debug!("Failed to sync entry file for indexing: {e}");
        }
    }

    client.await_indexing_signal().await;
}
