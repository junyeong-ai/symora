use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspClient;
use crate::infra::lsp::protocol::{Hover, TextDocumentIdentifier, TextDocumentPositionParams};
use crate::models::diagnostic::{Diagnostic, DiagnosticsReport, DiagnosticsStatus};
use crate::models::lsp::{HoverInfo, Indexed, SignatureHelp, path_to_uri};

use super::converters::*;
use super::helpers::*;
use super::position::PositionConverter;
use super::service::{DefaultLspService, degradation_of, ensure_indexed};

pub(super) async fn hover(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Option<HoverInfo>>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                ensure_indexed(&client, &file, manager.root()).await;
                let ran_under = degradation_of(client.indexing_state());

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(
                        line,
                        column,
                        &content,
                        client.position_encoding().await,
                    ),
                };

                let result: Option<Hover> = client
                    .request("textDocument/hover", Some(serde_json::to_value(params)?))
                    .await?;

                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);
                Ok(Indexed::new(
                    result.map(|h| {
                        let hover_text = extract_hover_content(&h.contents);
                        let range = h.range.map(|r| range_to_location(&file, &r, &mut conv));
                        HoverInfo {
                            content: hover_text,
                            range,
                        }
                    }),
                    ran_under,
                ))
            }
        })
        .await
}

pub(super) async fn signature_help(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Option<SignatureHelp>>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                ensure_indexed(&client, &file, manager.root()).await;
                let ran_under = degradation_of(client.indexing_state());

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(
                        line,
                        column,
                        &content,
                        client.position_encoding().await,
                    ),
                };

                let result: Option<serde_json::Value> = client
                    .request(
                        "textDocument/signatureHelp",
                        Some(serde_json::to_value(params)?),
                    )
                    .await?;

                let encoding = client.position_encoding().await;
                Ok(Indexed::new(
                    result.and_then(|v| parse_signature_help(&v, encoding)),
                    ran_under,
                ))
            }
        })
        .await
}

pub(super) async fn diagnostics(
    service: &DefaultLspService,
    file: &Path,
) -> Result<DiagnosticsReport, LspError> {
    use crate::infra::lsp::capabilities::{LspFeature, SupportLevel, get_support_level};
    use crate::infra::lsp::protocol::LspDiagnosticSeverity;
    use crate::infra::lsp::protocol::LspDiagnosticTag;
    use crate::models::diagnostic::{DiagnosticSeverity, DiagnosticTag};
    use crate::models::lsp::{Position as LspPosition, Range as LspRange};
    use crate::models::symbol::Language;

    // A language whose server doesn't publish diagnostics is an answer,
    // not a failure — callers attach the status inline instead of
    // branching on an error.
    let lang = Language::from_path(file);
    if get_support_level(lang, LspFeature::Diagnostics) == SupportLevel::None {
        return Ok(DiagnosticsReport::unsupported());
    }

    let max_file_size = service.max_file_size_bytes();
    let wait_budget = service.manager.runtime_config().diagnostics_wait;
    let file = file.to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                let version = client.sync_document(&uri, &content).await?;
                // Snapshot AFTER the sync: for servers that omit publish
                // versions, only arrivals strictly after the didChange went
                // out can count as fresh. An in-flight publish for the old
                // content must err toward Unconfirmed, never false-fresh.
                let seq_before = client.publish_seq_snapshot();

                let Some(lsp_diagnostics) =
                    wait_for_publish(&client, &uri, version, seq_before, wait_budget).await
                else {
                    return Ok(DiagnosticsReport {
                        status: DiagnosticsStatus::Unconfirmed,
                        items: Vec::new(),
                    });
                };

                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);
                let items = lsp_diagnostics
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
                                location: convert_location(&r.location, &mut conv),
                                message: r.message,
                            })
                            .collect();

                        Diagnostic {
                            file_path: file.display().to_string(),
                            range: LspRange {
                                start: LspPosition {
                                    line: d.range.start.line,
                                    character: conv.scalar_offset(
                                        &file,
                                        d.range.start.line,
                                        d.range.start.character,
                                    ),
                                },
                                end: LspPosition {
                                    line: d.range.end.line,
                                    character: conv.scalar_offset(
                                        &file,
                                        d.range.end.line,
                                        d.range.end.character,
                                    ),
                                },
                            },
                            severity,
                            message: d.message,
                            code: d.code.map(diagnostic_code_string),
                            source: d.source,
                            tags,
                            related_information,
                        }
                    })
                    .collect();

                Ok(DiagnosticsReport {
                    status: DiagnosticsStatus::Ok,
                    items,
                })
            }
        })
        .await
}

/// Wait for a `publishDiagnostics` that reflects the synced content.
///
/// A publish is fresh when the server stamps it with a document version
/// at or past the one we synced (LSP 3.15+), or — for servers that omit
/// versions — when it arrived after the sync began. A fresh publish
/// returns immediately, so clean files don't burn the window. `None`
/// means nothing fresh landed: the honest answer is "unconfirmed",
/// never a synthesized "clean".
async fn wait_for_publish(
    client: &LspClient,
    uri: &str,
    synced_version: u32,
    seq_before: u64,
    budget: std::time::Duration,
) -> Option<Vec<crate::infra::lsp::protocol::LspDiagnostic>> {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

    let start = std::time::Instant::now();
    loop {
        if let Some(published) = client.published_diagnostics(uri).await {
            let fresh = match published.doc_version {
                Some(v) => v >= synced_version,
                None => published.seq > seq_before,
            };
            if fresh {
                return Some(published.items);
            }
        }
        if start.elapsed() >= budget {
            return None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
