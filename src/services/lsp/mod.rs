mod cache;
mod converters;
mod deterministic;
mod editor;
pub(crate) mod helpers;
mod hierarchy;
mod info;
mod lifecycle;
mod navigation;
mod position;
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
    ApplyActionResult, CallHierarchyItem, CodeAction, CodeLens, Definition, FindSymbolsOptions,
    FoldingRange, HoverInfo, Indexed, InlayHint, PrepareRenameResult, RenameResult, SelectionRange,
    ServerStatus, SignatureHelp, TextEdit, TypeHierarchyItem,
};
use crate::models::symbol::{Language, Location, Symbol};

pub use deterministic::DeterministicLspService;
pub use service::DefaultLspService;

/// The LSP service every command speaks to. Workspace-dependent queries —
/// anything whose answer scales with how much of the workspace the server
/// has indexed (references, call/type hierarchies, implementations,
/// workspace symbols) — return [`Indexed`], pairing the data with the
/// indexing state it was computed under so output markers are derived
/// from a computation-time snapshot, never from a racy after-the-fact
/// read.
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
    ) -> Result<Indexed<Vec<Symbol>>, LspError>;

    async fn find_references(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<Location>>, LspError>;

    /// The definition the server names at a position — see
    /// [`Definition`]: a self-definition is disclosed, never presented as
    /// a definition that goes somewhere.
    async fn goto_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Option<Definition>>, LspError>;

    async fn goto_type_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Option<Location>>, LspError>;

    async fn find_implementations(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<Location>>, LspError>;

    async fn hover(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Option<HoverInfo>>, LspError>;

    async fn signature_help(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Option<SignatureHelp>>, LspError>;

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
    ) -> Result<Indexed<Option<RenameResult>>, LspError>;

    async fn incoming_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError>;

    async fn outgoing_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError>;

    async fn supertypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError>;

    async fn subtypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError>;

    /// Inlay hints across an inclusive line range. The surface is line-granular
    /// by contract — there are no column bounds — so the type cannot advertise a
    /// precision it does not honor; the implementation spans whole lines.
    async fn inlay_hints(
        &self,
        file: &Path,
        start_line: u32,
        end_line: u32,
    ) -> Result<Vec<InlayHint>, LspError>;

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

    /// Symora itself just wrote these files. Both implementations bring
    /// the language layer in line with the new bytes — invalidate per-file
    /// symbol caches, advance the workspace-content generation so cached
    /// workspace-wide answers can't outlive the write, and sync + save any
    /// live server's overlay (rust-analyzer re-checks on save) — without
    /// ever booting a server just to note an edit. Best-effort: an edit's
    /// success never depends on it.
    async fn note_files_edited(&self, _files: &[std::path::PathBuf]) {}
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
            ServerStatusDetail::CriticalFailure { reason, .. } => {
                ServerStatus::CriticalFailure { reason }
            }
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

    #[test]
    fn critical_failure_detail_maps_to_status_carrying_its_reason() {
        let detail = ServerStatusDetail::CriticalFailure {
            name: "rust-analyzer".to_string(),
            reason: "stayed unhealthy after 3 restart attempts".to_string(),
        };
        match ServerStatus::from(detail) {
            ServerStatus::CriticalFailure { reason } => {
                assert_eq!(reason, "stayed unhealthy after 3 restart attempts");
            }
            other => panic!("expected CriticalFailure, got {other:?}"),
        }
    }
}
