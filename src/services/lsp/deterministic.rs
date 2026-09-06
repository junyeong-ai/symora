use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::LspError;
use crate::models::diagnostic::DiagnosticsReport;
use crate::models::lsp::*;
use crate::models::symbol::{Language, Location, Symbol};

use super::LspService;

/// Confines answers to what this binary and the index carry.
///
/// A language server's answer depends on which servers a machine has, so two
/// checkouts of the same tree can disagree — the same file reads 26 symbols
/// where a server answered and 36 where the grammar did. A caller that gates
/// on the answer needs the reading that does not move: the index and the
/// compiled-in grammars, which derive from the tree alone.
///
/// So this declines every request that would produce an answer, and the
/// commands that have a second source take it — `symbols` reads the grammar,
/// `search symbols` the index — while the commands that have none fail rather
/// than degrade into something weaker wearing the same shape. Whether the
/// environment HAS a server is a different question from whether this answer
/// used one, so status and edit notification pass through: a warm server that
/// is never asked still must not outlive an edit this tool made.
pub struct DeterministicLspService {
    inner: Arc<dyn LspService + Send + Sync>,
}

impl DeterministicLspService {
    pub fn new(inner: Arc<dyn LspService + Send + Sync>) -> Self {
        Self { inner }
    }
}

fn declined<T>() -> Result<T, LspError> {
    Err(LspError::ServerNotConsulted)
}

#[async_trait]
impl LspService for DeterministicLspService {
    async fn find_symbols(
        &self,
        _file: &Path,
        _options: FindSymbolsOptions,
    ) -> Result<Vec<Symbol>, LspError> {
        declined()
    }

    async fn workspace_symbols(
        &self,
        _query: &str,
        _language: Language,
    ) -> Result<Indexed<Vec<Symbol>>, LspError> {
        declined()
    }

    async fn find_references(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Vec<Location>>, LspError> {
        declined()
    }

    async fn goto_definition(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Option<Definition>>, LspError> {
        declined()
    }

    async fn goto_type_definition(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Option<Location>>, LspError> {
        declined()
    }

    async fn find_implementations(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Vec<Location>>, LspError> {
        declined()
    }

    async fn hover(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Option<HoverInfo>>, LspError> {
        declined()
    }

    async fn signature_help(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Option<SignatureHelp>>, LspError> {
        declined()
    }

    async fn diagnostics(&self, _file: &Path) -> Result<DiagnosticsReport, LspError> {
        declined()
    }

    async fn prepare_rename(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Option<PrepareRenameResult>, LspError> {
        declined()
    }

    async fn rename(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
        _new_name: &str,
    ) -> Result<Indexed<Option<RenameResult>>, LspError> {
        declined()
    }

    async fn incoming_calls(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
        declined()
    }

    async fn outgoing_calls(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
        declined()
    }

    async fn supertypes(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
        declined()
    }

    async fn subtypes(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
        declined()
    }

    async fn inlay_hints(
        &self,
        _file: &Path,
        _start_line: u32,
        _end_line: u32,
    ) -> Result<Vec<InlayHint>, LspError> {
        declined()
    }

    async fn folding_ranges(&self, _file: &Path) -> Result<Vec<FoldingRange>, LspError> {
        declined()
    }

    async fn selection_ranges(
        &self,
        _file: &Path,
        _positions: Vec<(u32, u32)>,
    ) -> Result<Vec<SelectionRange>, LspError> {
        declined()
    }

    async fn code_lenses(&self, _file: &Path) -> Result<Vec<CodeLens>, LspError> {
        declined()
    }

    async fn code_actions(
        &self,
        _file: &Path,
        _line: u32,
        _column: u32,
    ) -> Result<Vec<CodeAction>, LspError> {
        declined()
    }

    async fn apply_code_action(
        &self,
        _file: &Path,
        _action: &CodeAction,
    ) -> Result<ApplyActionResult, LspError> {
        declined()
    }

    async fn format(&self, _file: &Path) -> Result<Vec<TextEdit>, LspError> {
        declined()
    }

    async fn is_available(&self, _language: Language) -> bool {
        false
    }

    async fn server_status(&self, language: Language) -> ServerStatus {
        self.inner.server_status(language).await
    }

    async fn note_files_edited(&self, files: &[std::path::PathBuf]) {
        self.inner.note_files_edited(files).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LspRuntimeConfig;
    use crate::services::lsp::DefaultLspService;

    fn service() -> DeterministicLspService {
        let inner = DefaultLspService::new(
            std::path::Path::new("/"),
            Arc::new(LspRuntimeConfig::default()),
        );
        DeterministicLspService::new(Arc::new(inner))
    }

    fn declined_by(name: &str, result: Result<(), LspError>) {
        match result {
            Err(LspError::ServerNotConsulted) => {}
            other => panic!("{name} did not decline: {other:?}"),
        }
    }

    /// Every method that produces an ANSWER declines, so the guarantee holds
    /// for the whole surface rather than the part someone remembered. A method
    /// added later and left to pass through fails here rather than quietly
    /// making one command's answer depend on the machine again.
    #[tokio::test]
    async fn every_answer_is_declined_and_only_environment_reads_pass_through() {
        let s = service();
        let f = std::path::Path::new("a.rs");

        declined_by(
            "find_symbols",
            s.find_symbols(f, FindSymbolsOptions::default())
                .await
                .map(|_| ()),
        );
        declined_by(
            "workspace_symbols",
            s.workspace_symbols("q", Language::Rust).await.map(|_| ()),
        );
        declined_by(
            "find_references",
            s.find_references(f, 1, 1).await.map(|_| ()),
        );
        declined_by(
            "goto_definition",
            s.goto_definition(f, 1, 1).await.map(|_| ()),
        );
        declined_by(
            "goto_type_definition",
            s.goto_type_definition(f, 1, 1).await.map(|_| ()),
        );
        declined_by(
            "find_implementations",
            s.find_implementations(f, 1, 1).await.map(|_| ()),
        );
        declined_by("hover", s.hover(f, 1, 1).await.map(|_| ()));
        declined_by(
            "signature_help",
            s.signature_help(f, 1, 1).await.map(|_| ()),
        );
        declined_by("diagnostics", s.diagnostics(f).await.map(|_| ()));
        declined_by(
            "prepare_rename",
            s.prepare_rename(f, 1, 1).await.map(|_| ()),
        );
        declined_by("rename", s.rename(f, 1, 1, "x").await.map(|_| ()));
        declined_by(
            "incoming_calls",
            s.incoming_calls(f, 1, 1).await.map(|_| ()),
        );
        declined_by(
            "outgoing_calls",
            s.outgoing_calls(f, 1, 1).await.map(|_| ()),
        );
        declined_by("supertypes", s.supertypes(f, 1, 1).await.map(|_| ()));
        declined_by("subtypes", s.subtypes(f, 1, 1).await.map(|_| ()));
        declined_by("inlay_hints", s.inlay_hints(f, 1, 2).await.map(|_| ()));
        declined_by("folding_ranges", s.folding_ranges(f).await.map(|_| ()));
        declined_by(
            "selection_ranges",
            s.selection_ranges(f, vec![(1, 1)]).await.map(|_| ()),
        );
        declined_by("code_lenses", s.code_lenses(f).await.map(|_| ()));
        declined_by("code_actions", s.code_actions(f, 1, 1).await.map(|_| ()));
        declined_by("format", s.format(f).await.map(|_| ()));
        let action = CodeAction {
            title: "t".to_string(),
            kind: CodeActionKind::default(),
            is_preferred: false,
            diagnostics: Vec::new(),
            edit: None,
            data: None,
        };
        declined_by(
            "apply_code_action",
            s.apply_code_action(f, &action).await.map(|_| ()),
        );

        assert!(
            !s.is_available(Language::Rust).await,
            "a confined run must not advertise a server it will refuse to ask"
        );
    }
}
