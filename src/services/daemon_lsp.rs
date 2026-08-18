use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::daemon::DaemonClient;
use crate::daemon::wire::{
    ApplyActionResponse, CallsResponse, CodeActionsResponse, CodeLensResponse, DefinitionResponse,
    DiagnosticsResponse, FoldingRangesResponse, FormatResponse, HoverResponse,
    ImplementationsResponse, InlayHintsResponse, PrepareRenameResponse, ReferencesResponse,
    RenameResponse, SelectionRangesResponse, SignatureResponse, SymbolsResponse,
    TypeHierarchyResponse,
};
use crate::error::LspError;
use crate::models::diagnostic::{Diagnostic, DiagnosticSeverity, DiagnosticTag, DiagnosticsReport};
use crate::models::lsp::{
    ApplyActionResult, CallHierarchyItem, CodeAction, CodeLens, Definition, FindSymbolsOptions,
    FoldingRange, HoverInfo, Indexed, InlayHint, Position, PrepareRenameResult, Range,
    RenameResult, SelectionRange, ServerStatus, SignatureHelp, TextEdit, TypeHierarchyItem,
};
use crate::models::symbol::{Language, Location, Symbol};
use crate::services::lsp::LspService;

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, LspError> {
    serde_json::from_value(value).map_err(|e| LspError::Protocol(e.to_string()))
}

pub struct DaemonLspService {
    client: DaemonClient,
}

impl DaemonLspService {
    pub fn new(project_root: &Path) -> Self {
        Self {
            client: DaemonClient::new(project_root),
        }
    }
}

#[async_trait]
impl LspService for DaemonLspService {
    async fn find_symbols(
        &self,
        file: &Path,
        options: FindSymbolsOptions,
    ) -> Result<Vec<Symbol>, LspError> {
        let result = self
            .client
            .find_symbols(file, options.include_body, options.depth)
            .await?;

        let response: SymbolsResponse = parse(result)?;

        Ok(response.symbols.into_iter().map(Symbol::from).collect())
    }

    async fn workspace_symbols(
        &self,
        query: &str,
        language: Language,
    ) -> Result<Indexed<Vec<Symbol>>, LspError> {
        let result = self
            .client
            .workspace_symbols(query, &language.to_string())
            .await?;

        let response: SymbolsResponse = parse(result)?;

        Ok(Indexed::new(
            response.symbols.into_iter().map(Symbol::from).collect(),
            response.indexing,
        ))
    }

    async fn find_references(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<Location>>, LspError> {
        let result = self.client.find_references(file, line, column).await?;

        let response: ReferencesResponse = parse(result)?;

        Ok(Indexed::new(
            response.references.into_iter().map(Into::into).collect(),
            response.indexing,
        ))
    }

    async fn goto_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Definition>, LspError> {
        let result = self.client.goto_definition(file, line, column).await?;

        let response: DefinitionResponse = parse(result)?;

        let is_self = response.is_self;
        Ok(response.definition.map(|location| Definition {
            location: location.into(),
            is_self,
        }))
    }

    async fn goto_type_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Location>, LspError> {
        let result = self.client.goto_type_definition(file, line, column).await?;

        let response: DefinitionResponse = parse(result)?;

        Ok(response.definition.map(Into::into))
    }

    async fn find_implementations(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<Location>>, LspError> {
        let result = self.client.find_implementations(file, line, column).await?;

        let response: ImplementationsResponse = parse(result)?;

        Ok(Indexed::new(
            response
                .implementations
                .into_iter()
                .map(Into::into)
                .collect(),
            response.indexing,
        ))
    }

    async fn hover(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<HoverInfo>, LspError> {
        let result = self.client.hover(file, line, column).await?;

        let response: HoverResponse = parse(result)?;

        Ok(response
            .content
            .filter(|c| !c.is_empty())
            .map(|content| HoverInfo {
                content,
                range: response.range.map(Into::into),
            }))
    }

    async fn signature_help(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<SignatureHelp>, LspError> {
        let result = self.client.signature_help(file, line, column).await?;

        let response: SignatureResponse = parse(result)?;

        if response.signatures.is_empty() {
            return Ok(None);
        }

        Ok(Some(SignatureHelp {
            signatures: response.signatures.into_iter().map(Into::into).collect(),
            active_signature: response.active_signature,
            active_parameter: response.active_parameter,
        }))
    }

    async fn diagnostics(&self, file: &Path) -> Result<DiagnosticsReport, LspError> {
        let result = self.client.diagnostics(file).await?;

        let response: DiagnosticsResponse = parse(result)?;

        let items = response
            .diagnostics
            .into_iter()
            .map(|d| {
                let line = d.line.saturating_sub(1);
                let column = d.column.saturating_sub(1);
                let end_line = d.end_line.saturating_sub(1);
                let end_column = d.end_column.saturating_sub(1);
                Diagnostic {
                    file_path: file.display().to_string(),
                    message: d.message,
                    severity: d.severity.parse().unwrap_or(DiagnosticSeverity::Hint),
                    range: Range {
                        start: Position::new(line, column),
                        end: Position::new(end_line, end_column),
                    },
                    source: d.source,
                    code: d.code,
                    tags: d
                        .tags
                        .iter()
                        .filter_map(|t| match t.as_str() {
                            "unnecessary" => Some(DiagnosticTag::Unnecessary),
                            "deprecated" => Some(DiagnosticTag::Deprecated),
                            _ => None,
                        })
                        .collect(),
                    related_information: d
                        .related_information
                        .iter()
                        .map(|ri| crate::models::diagnostic::DiagnosticRelatedInfo {
                            location: crate::models::symbol::Location::point(
                                PathBuf::from(&ri.file),
                                ri.line,
                                ri.column,
                            )
                            .with_degraded_column(ri.degraded_column == Some(true)),
                            message: ri.message.clone(),
                        })
                        .collect(),
                }
            })
            .collect();

        Ok(DiagnosticsReport {
            status: response.status,
            items,
        })
    }

    async fn prepare_rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<PrepareRenameResult>, LspError> {
        let result = self.client.prepare_rename(file, line, column).await?;

        let response: PrepareRenameResponse = parse(result)?;

        Ok(response
            .placeholder
            .map(|placeholder| PrepareRenameResult { placeholder }))
    }

    async fn rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
        new_name: &str,
    ) -> Result<Option<RenameResult>, LspError> {
        let result = self.client.rename(file, line, column, new_name).await?;

        let response: RenameResponse = parse(result)?;

        Ok(response.changes.map(|changes| RenameResult {
            changes: changes.into_iter().map(Into::into).collect(),
        }))
    }

    async fn incoming_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
        let result = self.client.incoming_calls(file, line, column).await?;

        let response: CallsResponse = parse(result)?;

        Ok(Indexed::new(
            response.calls.into_iter().map(Into::into).collect(),
            response.indexing,
        ))
    }

    async fn outgoing_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
        let result = self.client.outgoing_calls(file, line, column).await?;

        let response: CallsResponse = parse(result)?;

        Ok(Indexed::new(
            response.calls.into_iter().map(Into::into).collect(),
            response.indexing,
        ))
    }

    async fn supertypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
        let result = self.client.supertypes(file, line, column).await?;
        let response: TypeHierarchyResponse = parse(result)?;
        Ok(Indexed::new(
            response.items.into_iter().map(Into::into).collect(),
            response.indexing,
        ))
    }

    async fn subtypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
        let result = self.client.subtypes(file, line, column).await?;
        let response: TypeHierarchyResponse = parse(result)?;
        Ok(Indexed::new(
            response.items.into_iter().map(Into::into).collect(),
            response.indexing,
        ))
    }

    async fn inlay_hints(
        &self,
        file: &Path,
        start_line: u32,
        end_line: u32,
    ) -> Result<Vec<InlayHint>, LspError> {
        let result = self.client.inlay_hints(file, start_line, end_line).await?;
        let response: InlayHintsResponse = parse(result)?;
        Ok(response.hints.into_iter().map(Into::into).collect())
    }

    async fn folding_ranges(&self, file: &Path) -> Result<Vec<FoldingRange>, LspError> {
        let result = self.client.folding_ranges(file).await?;
        let response: FoldingRangesResponse = parse(result)?;
        Ok(response.ranges.into_iter().map(Into::into).collect())
    }

    async fn selection_ranges(
        &self,
        file: &Path,
        positions: Vec<(u32, u32)>,
    ) -> Result<Vec<SelectionRange>, LspError> {
        let result = self.client.selection_ranges(file, &positions).await?;
        let response: SelectionRangesResponse = parse(result)?;
        Ok(response.ranges.into_iter().map(Into::into).collect())
    }

    async fn code_lenses(&self, file: &Path) -> Result<Vec<CodeLens>, LspError> {
        let result = self.client.code_lenses(file).await?;
        let response: CodeLensResponse = parse(result)?;
        Ok(response.lenses.into_iter().map(Into::into).collect())
    }

    async fn code_actions(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CodeAction>, LspError> {
        let result = self.client.code_actions(file, line, column).await?;

        let response: CodeActionsResponse = parse(result)?;

        Ok(response.actions.into_iter().map(Into::into).collect())
    }

    async fn apply_code_action(
        &self,
        file: &Path,
        action: &CodeAction,
    ) -> Result<ApplyActionResult, LspError> {
        let action_json = serde_json::to_value(action)
            .map_err(|e| LspError::Protocol(format!("Failed to serialize action: {}", e)))?;

        let result = self.client.apply_code_action(file, &action_json).await?;

        let response: ApplyActionResponse = parse(result)?;

        Ok(ApplyActionResult {
            changes: response.changes.into_iter().map(Into::into).collect(),
        })
    }

    async fn format(&self, file: &Path) -> Result<Vec<TextEdit>, LspError> {
        let result = self.client.format(file).await?;
        let response: FormatResponse = parse(result)?;
        Ok(response.edits.into_iter().map(Into::into).collect())
    }

    async fn is_available(&self, language: Language) -> bool {
        match self.client.language_status(&language.to_string()).await {
            Ok(v) => v
                .get("available")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    async fn server_status(&self, language: Language) -> ServerStatus {
        match self.client.language_status(&language.to_string()).await {
            Ok(v) => {
                let status = v
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("stopped");
                match status {
                    "running" => ServerStatus::Running,
                    "not_installed" => ServerStatus::NotInstalled {
                        hint: v
                            .get("install_hint")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    },
                    "not_supported" => ServerStatus::NotSupported,
                    "critical_failure" => ServerStatus::CriticalFailure {
                        reason: v
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("server repeatedly failed to start")
                            .to_string(),
                    },
                    _ => ServerStatus::Stopped,
                }
            }
            Err(_) => ServerStatus::Stopped,
        }
    }

    /// Forward the post-edit note to the daemon, where the caches and
    /// overlays live. Best-effort by contract: a daemon that is not
    /// running has nothing to catch up, and a forwarding failure must
    /// not fail the edit that triggered it.
    async fn note_files_edited(&self, files: &[PathBuf]) {
        if let Err(e) = self.client.note_files_edited(files).await {
            tracing::warn!("Post-edit LSP note failed: {e}");
        }
    }
}
