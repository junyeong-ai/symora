//! Daemon-backed LSP Service Implementation

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::daemon::DaemonClient;
use crate::daemon::protocol::dto::{
    ApplyActionResponse, CallsResponse, CodeActionsResponse, CodeLensResponse, DefinitionResponse,
    DiagnosticsResponse, FoldingRangesResponse, HoverResponse, ImplementationsResponse,
    InlayHintsResponse, PrepareRenameResponse, ReferencesResponse, RenameResponse,
    SelectionRangesResponse, SignatureResponse, SymbolsResponse, TypeHierarchyResponse,
};
use crate::error::LspError;
use crate::models::diagnostic::{Diagnostic, DiagnosticSeverity};
use crate::models::lsp::{
    ApplyActionResult, CallHierarchyItem, CodeAction, CodeActionKind, CodeLens, CodeLensCommand,
    FileChange, FileChangeWithEdits, FindSymbolsOptions, FoldingRange, FoldingRangeKind, HoverInfo,
    InlayHint, InlayHintKind, ParameterInfo, Position, PrepareRenameResult, Range, RenameResult,
    SelectionRange, ServerStatus, SignatureHelp, SignatureInfo, TextEdit, TypeHierarchyItem,
};
use crate::models::symbol::{Language, Location, Symbol, SymbolKind};
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
            .find_symbols_with_options(file, options.include_body, options.depth)
            .await?;

        let response: SymbolsResponse = parse(result)?;

        Ok(response
            .symbols
            .into_iter()
            .map(|dto| dto.into_symbol())
            .collect())
    }

    async fn workspace_symbols(
        &self,
        query: &str,
        language: Language,
    ) -> Result<Vec<Symbol>, LspError> {
        let result = self
            .client
            .workspace_symbols(query, &language.to_string())
            .await?;

        let response: SymbolsResponse = parse(result)?;

        let mut seen = std::collections::HashSet::new();
        Ok(response
            .symbols
            .into_iter()
            .filter_map(|dto| {
                let key = (
                    dto.name.clone(),
                    dto.kind.clone(),
                    dto.file.clone(),
                    dto.line,
                    dto.column,
                );
                if seen.insert(key) {
                    Some(dto.into_symbol())
                } else {
                    None
                }
            })
            .collect())
    }

    async fn find_references(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError> {
        let result = self.client.find_references(file, line, column).await?;

        let response: ReferencesResponse = parse(result)?;

        Ok(response.references.into_iter().map(Into::into).collect())
    }

    async fn goto_definition(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<Location>, LspError> {
        let result = self.client.goto_definition(file, line, column).await?;

        let response: DefinitionResponse = parse(result)?;

        Ok(response.definition.map(Into::into))
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
    ) -> Result<Vec<Location>, LspError> {
        let result = self.client.find_implementations(file, line, column).await?;

        let response: ImplementationsResponse = parse(result)?;

        Ok(response
            .implementations
            .into_iter()
            .map(Into::into)
            .collect())
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
                range: None,
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
            signatures: response
                .signatures
                .into_iter()
                .map(|s| SignatureInfo {
                    label: s.label,
                    documentation: s.documentation,
                    parameters: s
                        .parameters
                        .into_iter()
                        .map(|p| ParameterInfo {
                            label: p.label,
                            documentation: p.documentation,
                        })
                        .collect(),
                    active_parameter: s.active_parameter,
                })
                .collect(),
            active_signature: response.active_signature,
            active_parameter: response.active_parameter,
        }))
    }

    async fn diagnostics(&self, file: &Path) -> Result<Vec<Diagnostic>, LspError> {
        let result = self.client.diagnostics(file).await?;

        let response: DiagnosticsResponse = parse(result)?;

        Ok(response
            .diagnostics
            .into_iter()
            .map(|d| {
                let line = d.line.saturating_sub(1);
                let column = d.column.saturating_sub(1);
                Diagnostic {
                    file_path: file.display().to_string(),
                    message: d.message,
                    severity: match d.severity.as_str() {
                        "Error" => DiagnosticSeverity::Error,
                        "Warning" => DiagnosticSeverity::Warning,
                        "Information" => DiagnosticSeverity::Information,
                        _ => DiagnosticSeverity::Hint,
                    },
                    range: Range {
                        start: Position::new(line, column),
                        end: Position::new(line, column + 1),
                    },
                    source: None,
                    code: None,
                    tags: vec![],
                    related_information: vec![],
                }
            })
            .collect())
    }

    async fn prepare_rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<PrepareRenameResult>, LspError> {
        let result = self.client.prepare_rename(file, line, column).await?;

        let response: PrepareRenameResponse = parse(result)?;

        match (response.placeholder, response.range) {
            (Some(placeholder), Some(range)) => Ok(Some(PrepareRenameResult {
                placeholder,
                range: range.into(),
            })),
            _ => Ok(None),
        }
    }

    async fn rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
        new_name: &str,
    ) -> Result<RenameResult, LspError> {
        let result = self.client.rename(file, line, column, new_name).await?;

        let response: RenameResponse = parse(result)?;

        Ok(RenameResult {
            changes: response
                .changes
                .into_iter()
                .map(|c| FileChange {
                    file: PathBuf::from(c.file),
                    edit_count: c.edit_count,
                })
                .collect(),
        })
    }

    async fn incoming_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError> {
        let result = self.client.incoming_calls(file, line, column).await?;

        let response: CallsResponse = parse(result)?;

        Ok(response
            .calls
            .into_iter()
            .map(|c| CallHierarchyItem {
                name: c.name,
                kind: SymbolKind::from_str_loose(&c.kind),
                location: Location::point(PathBuf::from(&c.file), c.line, c.column),
                call_site: c
                    .call_site
                    .map(|cs| Location::point(PathBuf::from(cs.file), cs.line, cs.column)),
            })
            .collect())
    }

    async fn outgoing_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError> {
        let result = self.client.outgoing_calls(file, line, column).await?;

        let response: CallsResponse = parse(result)?;

        Ok(response
            .calls
            .into_iter()
            .map(|c| CallHierarchyItem {
                name: c.name,
                kind: SymbolKind::from_str_loose(&c.kind),
                location: Location::point(PathBuf::from(&c.file), c.line, c.column),
                call_site: c
                    .call_site
                    .map(|cs| Location::point(PathBuf::from(cs.file), cs.line, cs.column)),
            })
            .collect())
    }

    async fn supertypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<TypeHierarchyItem>, LspError> {
        let result = self.client.supertypes(file, line, column).await?;
        let response: TypeHierarchyResponse = parse(result)?;
        Ok(response
            .items
            .into_iter()
            .map(|item| TypeHierarchyItem {
                name: item.name,
                kind: SymbolKind::from_str_loose(&item.kind),
                location: Location::point(PathBuf::from(item.file), item.line, item.column),
                detail: item.detail,
            })
            .collect())
    }

    async fn subtypes(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<TypeHierarchyItem>, LspError> {
        let result = self.client.subtypes(file, line, column).await?;
        let response: TypeHierarchyResponse = parse(result)?;
        Ok(response
            .items
            .into_iter()
            .map(|item| TypeHierarchyItem {
                name: item.name,
                kind: SymbolKind::from_str_loose(&item.kind),
                location: Location::point(PathBuf::from(item.file), item.line, item.column),
                detail: item.detail,
            })
            .collect())
    }

    async fn inlay_hints(&self, file: &Path, range: Range) -> Result<Vec<InlayHint>, LspError> {
        let result = self
            .client
            .inlay_hints(
                file,
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character,
            )
            .await?;
        let response: InlayHintsResponse = parse(result)?;
        Ok(response
            .hints
            .into_iter()
            .map(|h| InlayHint {
                position: Position::new(h.line, h.character),
                label: h.label,
                kind: InlayHintKind::from_lsp(h.kind),
                padding_left: h.padding_left,
                padding_right: h.padding_right,
            })
            .collect())
    }

    async fn folding_ranges(&self, file: &Path) -> Result<Vec<FoldingRange>, LspError> {
        let result = self.client.folding_ranges(file).await?;
        let response: FoldingRangesResponse = parse(result)?;
        Ok(response
            .ranges
            .into_iter()
            .map(|r| FoldingRange {
                start_line: r.start_line,
                end_line: r.end_line,
                start_character: r.start_character,
                end_character: r.end_character,
                kind: FoldingRangeKind::from_lsp(r.kind.as_deref()),
                collapsed_text: r.collapsed_text,
            })
            .collect())
    }

    async fn selection_ranges(
        &self,
        file: &Path,
        positions: Vec<(u32, u32)>,
    ) -> Result<Vec<SelectionRange>, LspError> {
        let result = self.client.selection_ranges(file, &positions).await?;
        let response: SelectionRangesResponse = parse(result)?;

        fn convert_selection_range(
            dto: &crate::daemon::protocol::dto::SelectionRangeDto,
        ) -> SelectionRange {
            SelectionRange {
                range: Range::new(
                    Position::new(dto.start_line, dto.start_character),
                    Position::new(dto.end_line, dto.end_character),
                ),
                parent: dto
                    .parent
                    .as_ref()
                    .map(|p| Box::new(convert_selection_range(p))),
            }
        }

        Ok(response
            .ranges
            .iter()
            .map(convert_selection_range)
            .collect())
    }

    async fn code_lens(&self, file: &Path) -> Result<Vec<CodeLens>, LspError> {
        let result = self.client.code_lens(file).await?;
        let response: CodeLensResponse = parse(result)?;
        Ok(response
            .lenses
            .into_iter()
            .map(|lens| CodeLens {
                range: Range::new(
                    Position::new(lens.start_line, lens.start_character),
                    Position::new(lens.end_line, lens.end_character),
                ),
                command: lens.command.map(|cmd| CodeLensCommand {
                    title: cmd.title,
                    command: cmd.command,
                    arguments: cmd.arguments,
                }),
                data: lens.data,
            })
            .collect())
    }

    async fn code_actions(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<CodeAction>, LspError> {
        let result = self.client.code_actions(file, line, column).await?;

        let response: CodeActionsResponse = parse(result)?;

        Ok(response
            .actions
            .into_iter()
            .map(|a| CodeAction {
                title: a.title,
                kind: CodeActionKind::from(a.kind.as_deref()),
                is_preferred: a.is_preferred,
                diagnostics: a.diagnostics,
                edit: None,
                data: None,
            })
            .collect())
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
            changes: response
                .changes
                .into_iter()
                .map(|c| FileChangeWithEdits {
                    file: PathBuf::from(c.file),
                    edits: c
                        .edits
                        .into_iter()
                        .map(|e| TextEdit {
                            range: e.range.into(),
                            new_text: e.new_text,
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    async fn is_available(&self, _language: Language) -> bool {
        self.client.status().await.is_ok()
    }

    async fn server_status(&self, _language: Language) -> ServerStatus {
        match self.client.status().await {
            Ok(_) => ServerStatus::Running,
            Err(_) => ServerStatus::Stopped,
        }
    }

    async fn shutdown(&self) {
        // Daemon handles LSP server lifecycle
    }

    async fn cleanup_idle(&self, _timeout: std::time::Duration) -> usize {
        // Daemon server handles idle cleanup internally
        0
    }
}
