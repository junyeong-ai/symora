use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspClient;
use crate::infra::lsp::protocol::{Hover, TextDocumentIdentifier, TextDocumentPositionParams};
use crate::models::diagnostic::{Diagnostic, DiagnosticsReport, DiagnosticsStatus};
use crate::models::lsp::{HoverInfo, Indexed, path_to_uri};

use super::converters::*;
use super::helpers::*;
use super::position::PositionConverter;
use super::service::{DefaultLspService, ensure_indexed};

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
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;
                let waited = ensure_indexed(&client, &file, manager.root()).await;
                let indexing = client.indexing_query(waited);

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
                Ok(indexing.finish(result.map(|h| {
                    let hover_text = extract_hover_content(&h.contents);
                    let range = h.range.map(|r| range_to_location(&file, &r, &mut conv));
                    HoverInfo {
                        content: hover_text,
                        range,
                    }
                })))
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

    // A language whose server provides no diagnostics is an answer,
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
                let version = client.sync_document_for_diagnostics(&uri, &content).await?;
                // Snapshot AFTER the sync: for servers that omit publish
                // versions, only arrivals strictly after the didChange went
                // out can count as fresh. An in-flight publish for the old
                // content must err toward Unconfirmed, never false-fresh.
                let seq_before = client.publish_seq_snapshot();

                let Some(lsp_diagnostics) =
                    read_fresh_diagnostics(&client, &uri, version, seq_before, wait_budget).await?
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

/// Wait for diagnostics that reflect the synced content: the answer to a
/// pull, from a server that serves pulls for the document, or else a fresh
/// publish.
///
/// A publish is fresh when the server stamps it with a document version
/// at or past the one we synced (LSP 3.15+), or — for servers that omit
/// versions — when it arrived after the sync began. An answer returns
/// immediately, so clean files don't burn the window. `None` means no
/// answer for the synced content came within it: the honest answer is
/// "unconfirmed", never a synthesized "clean".
async fn read_fresh_diagnostics(
    client: &LspClient,
    uri: &str,
    synced_version: u32,
    seq_before: u64,
    budget: std::time::Duration,
) -> Result<Option<Vec<crate::infra::lsp::protocol::LspDiagnostic>>, LspError> {
    use crate::infra::lsp::DiagnosticPull;
    use tokio::time::{Duration, Instant, sleep};

    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let deadline = Instant::now() + budget;
    loop {
        match client.pull_diagnostics(uri).await? {
            DiagnosticPull::Answered(items) => return Ok(Some(items)),
            DiagnosticPull::Unanalyzed => return Ok(None),
            DiagnosticPull::Pending => {}
            DiagnosticPull::NotServed => {
                if let Some(published) = client.published_diagnostics(uri).await {
                    let fresh = match published.doc_version {
                        Some(v) => v >= synced_version,
                        None => published.seq > seq_before,
                    };
                    if fresh {
                        return Ok(Some(published.items));
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(POLL_INTERVAL).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::infra::lsp::servers::ServerLaunch;
    use crate::models::symbol::Language;

    async fn pull_server(root: &Path, mode: &str) -> Arc<LspClient> {
        let client = LspClient::new(
            Language::Python,
            root.to_path_buf(),
            Arc::new(crate::config::LspRuntimeConfig::default()),
            false,
        );
        let launch = ServerLaunch {
            command: PathBuf::from("python3"),
            args: vec![
                "-u".into(),
                "-c".into(),
                include_str!("../../../tests/fixtures/lsp/pull_server.py").into(),
                mode.into(),
            ],
            env: Default::default(),
        };
        client.start(&launch, || {}).await.unwrap();
        client
    }

    /// The lines a read reports errors on after `content` is written, or
    /// `None` when the read stays unconfirmed.
    async fn read(
        client: &LspClient,
        path: &Path,
        content: &str,
        budget: Duration,
    ) -> Option<Vec<u32>> {
        std::fs::write(path, content).unwrap();
        let uri = path_to_uri(path);
        let version = client
            .sync_document_for_diagnostics(&uri, content)
            .await
            .unwrap();
        let seq_before = client.publish_seq_snapshot();
        read_fresh_diagnostics(client, &uri, version, seq_before, budget)
            .await
            .unwrap()
            .map(|items| items.iter().map(|item| item.range.start.line).collect())
    }

    #[tokio::test]
    async fn a_registered_pull_answers_for_the_current_content() {
        let root = tempfile::tempdir().unwrap();
        let client = pull_server(root.path(), "registered").await;
        let file = root.path().join("a.py");
        let budget = Duration::from_secs(10);
        assert_eq!(
            read(&client, &file, "error\nok\n", budget).await,
            Some(vec![0])
        );
        assert_eq!(
            read(&client, &file, "ok\nerror\n", budget).await,
            Some(vec![1])
        );
        client.shutdown().await.unwrap();
    }

    /// A pull is a request the server answers once it has read the content,
    /// so its answer counts whenever it comes: the window bounds only waiting
    /// on what the server sends of its own accord.
    #[tokio::test]
    async fn a_pull_answered_past_the_window_counts() {
        let root = tempfile::tempdir().unwrap();
        let client = pull_server(root.path(), "registered").await;
        let file = root.path().join("a.py");
        let short = Duration::from_millis(100);
        assert_eq!(
            read(&client, &file, "error\nslow\n", short).await,
            Some(vec![0])
        );
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_pull_counts_only_while_the_workspace_is_reported_ready() {
        let root = tempfile::tempdir().unwrap();
        let client = pull_server(root.path(), "workspace").await;
        let file = root.path().join("a.rs");
        let short = Duration::from_millis(300);
        let budget = Duration::from_secs(10);
        assert_eq!(read(&client, &file, "error\n", short).await, None);
        assert_eq!(
            read(&client, &file, "error\nready\n", budget).await,
            Some(vec![0])
        );
        assert_eq!(read(&client, &file, "error\nfailed\n", short).await, None);
        assert_eq!(read(&client, &file, "error\ndegraded\n", short).await, None);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_pull_answered_across_a_reload_or_cancelled_is_asked_again() {
        let root = tempfile::tempdir().unwrap();
        let client = pull_server(root.path(), "workspace").await;
        let file = root.path().join("a.rs");
        let budget = Duration::from_secs(10);
        for content in ["error\nready\nreload\n", "error\nready\ncancel\n"] {
            assert_eq!(read(&client, &file, content, budget).await, Some(vec![0]));
        }
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_empty_pull_counts_only_for_a_document_in_a_project() {
        let root = tempfile::tempdir().unwrap();
        let client = pull_server(root.path(), "workspace").await;
        let file = root.path().join("a.rs");
        let budget = Duration::from_secs(10);
        let started = std::time::Instant::now();
        assert_eq!(read(&client, &file, "ready\nlibrary\n", budget).await, None);
        assert!(started.elapsed() < budget / 2, "{:?}", started.elapsed());
        assert_eq!(
            read(&client, &file, "error\nready\nlibrary\n", budget).await,
            Some(vec![0])
        );
        assert_eq!(read(&client, &file, "ready\n", budget).await, Some(vec![]));
        client.shutdown().await.unwrap();
    }
}
