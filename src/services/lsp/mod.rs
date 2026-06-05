mod cache;
mod converters;
mod editor;
pub(crate) mod helpers;
mod hierarchy;
mod info;
mod lifecycle;
mod navigation;
mod rename;
mod service;
mod symbols;

pub use helpers::find_containing_callable;

use std::path::Path;

use async_trait::async_trait;

use crate::error::LspError;
use crate::infra::lsp::ServerStatusDetail;
use crate::models::diagnostic::DiagnosticsReport;
use crate::models::lsp::{
    ApplyActionResult, CallHierarchyItem, CodeAction, CodeLens, FindSymbolsOptions, FoldingRange,
    HoverInfo, IndexingDegradation, InlayHint, PrepareRenameResult, Range, RenameResult,
    SelectionRange, ServerStatus, SignatureHelp, TextEdit, TypeHierarchyItem,
};
use crate::models::symbol::{Language, Location, Symbol};

pub use service::DefaultLspService;

#[async_trait]
pub trait LspService: Send + Sync {
    async fn find_symbols(
        &self,
        file: &Path,
        options: FindSymbolsOptions,
    ) -> Result<Vec<Symbol>, LspError>;

    async fn workspace_symbols(
        &self,
        query: &str,
        language: Language,
    ) -> Result<Vec<Symbol>, LspError>;

    async fn find_references(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError>;

    async fn goto_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Location>, LspError>;

    async fn goto_type_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Location>, LspError>;

    async fn find_implementations(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError>;

    async fn hover(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<HoverInfo>, LspError>;

    async fn signature_help(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<SignatureHelp>, LspError>;

    async fn diagnostics(&self, file: &Path) -> Result<DiagnosticsReport, LspError>;

    async fn prepare_rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<PrepareRenameResult>, LspError>;

    async fn rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
        new_name: &str,
    ) -> Result<RenameResult, LspError>;

    async fn incoming_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError>;

    async fn outgoing_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError>;

    async fn supertypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<TypeHierarchyItem>, LspError>;

    async fn subtypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<TypeHierarchyItem>, LspError>;

    async fn inlay_hints(&self, file: &Path, range: Range) -> Result<Vec<InlayHint>, LspError>;

    async fn folding_ranges(&self, file: &Path) -> Result<Vec<FoldingRange>, LspError>;

    async fn selection_ranges(
        &self,
        file: &Path,
        positions: Vec<(u32, u32)>,
    ) -> Result<Vec<SelectionRange>, LspError>;

    async fn code_lenses(&self, file: &Path) -> Result<Vec<CodeLens>, LspError>;

    async fn code_actions(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CodeAction>, LspError>;

    async fn apply_code_action(
        &self,
        file: &Path,
        action: &CodeAction,
    ) -> Result<ApplyActionResult, LspError>;

    async fn format(&self, file: &Path) -> Result<Vec<TextEdit>, LspError>;

    async fn is_available(&self, language: Language) -> bool;

    async fn server_status(&self, language: Language) -> ServerStatus;

    /// Whether the language's workspace indexing is in a degraded state
    /// (latched on the running session by `await_indexing_signal`). Commands
    /// whose answers depend on workspace indexing query this after their
    /// data call and mark the response, so an agent never mistakes a
    /// cold-server lower bound for a complete answer.
    async fn indexing_degradation(&self, language: Language) -> Option<IndexingDegradation>;
}

impl From<ServerStatusDetail> for ServerStatus {
    fn from(status: ServerStatusDetail) -> Self {
        match status {
            ServerStatusDetail::Running { .. } => ServerStatus::Running,
            ServerStatusDetail::Stopped { .. } => ServerStatus::Stopped,
            ServerStatusDetail::NotInstalled { install_hint, .. } => ServerStatus::NotInstalled {
                hint: Some(install_hint),
            },
            ServerStatusDetail::NotSupported => ServerStatus::NotSupported,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::lsp::{path_to_uri, uri_to_path};
    use std::path::PathBuf;

    #[test]
    fn test_path_to_uri() {
        let uri = path_to_uri(Path::new("/test/file.rs"));
        assert!(uri.starts_with("file://"));
        assert!(uri.contains("file.rs"));
    }

    #[test]
    fn test_uri_to_path() {
        let path = uri_to_path("file:///test/file.rs");
        assert_eq!(path, PathBuf::from("/test/file.rs"));
    }
}
