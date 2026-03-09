use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspClient;
use crate::infra::lsp::protocol::{Hover, TextDocumentIdentifier, TextDocumentPositionParams};
use crate::models::diagnostic::Diagnostic;
use crate::models::lsp::{HoverInfo, SignatureHelp, path_to_uri};

use super::converters::*;
use super::helpers::*;
use super::service::{DefaultLspService, ensure_indexed};

pub(super) async fn hover(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Option<HoverInfo>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                ensure_indexed(&client, &file, manager.root()).await;

                let content = read_file_validated(&file, max_file_size).await?;
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

pub(super) async fn signature_help(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Option<SignatureHelp>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                ensure_indexed(&client, &file, manager.root()).await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

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
        })
        .await
}

pub(super) async fn diagnostics(
    service: &DefaultLspService,
    file: &Path,
) -> Result<Vec<Diagnostic>, LspError> {
    use crate::infra::lsp::protocol::LspDiagnosticSeverity;
    use crate::infra::lsp::protocol::LspDiagnosticTag;
    use crate::models::diagnostic::{DiagnosticSeverity, DiagnosticTag};
    use crate::models::lsp::{Position as LspPosition, Range as LspRange};

    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let lsp_diagnostics = wait_for_diagnostics(&client, &uri).await;

                let diagnostics = lsp_diagnostics
                    .into_iter()
                    .map(|d| {
                        let severity = match d.severity {
                            Some(s) => match s {
                                LspDiagnosticSeverity::Error => DiagnosticSeverity::Error,
                                LspDiagnosticSeverity::Warning => DiagnosticSeverity::Warning,
                                LspDiagnosticSeverity::Information => {
                                    DiagnosticSeverity::Information
                                }
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
        })
        .await
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
