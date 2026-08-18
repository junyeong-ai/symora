use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, watch};

use serde::Serialize;

use crate::config::LspRuntimeConfig;
use crate::daemon::protocol::{Request, RequestId, Response, RpcError, methods};

use super::config::DaemonRuntimeConfig;
use super::context::ProjectsMap;
use super::dispatch::dispatch;

fn serialize_response(response: &impl Serialize) -> String {
    serde_json::to_string(response).unwrap_or_else(|e| {
        tracing::error!("Failed to serialize response: {}", e);
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"Serialization error"}}"#
            .to_string()
    })
}

pub(super) async fn handle_connection(
    stream: UnixStream,
    projects: ProjectsMap,
    semaphore: Arc<Semaphore>,
    config: Arc<DaemonRuntimeConfig>,
    lsp_config: Arc<LspRuntimeConfig>,
    start_time: Instant,
    shutdown: watch::Sender<bool>,
) -> Result<(), std::io::Error> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    const MAX_LINE_SIZE: usize = 10 * 1024 * 1024; // 10MB

    // Shutdown releases the listener, so no NEW connection reaches this
    // process — but a connection already accepted would keep serving
    // requests from it, and a replacement is serving the socket by then.
    // Two daemons answering for the same projects is the state the socket
    // claim exists to prevent, so an established connection stops taking
    // work at the same moment a new one stops arriving. A request already
    // dispatched still gets its answer; only the next one is declined.
    let mut shutdown_rx = shutdown.subscribe();
    loop {
        let mut line_buf = Vec::with_capacity(4096);
        let mut bounded = (&mut reader).take(MAX_LINE_SIZE as u64 + 1);
        let bytes_read = tokio::select! {
            biased;
            _ = shutdown_rx.wait_for(|stopping| *stopping) => break,
            read = bounded.read_until(b'\n', &mut line_buf) => read?,
        };
        if bytes_read == 0 {
            break;
        }
        if line_buf.len() > MAX_LINE_SIZE {
            // Drain remaining bytes until newline to prevent corrupted next read
            if !line_buf.ends_with(b"\n") {
                let mut drain = Vec::new();
                let _ = reader.read_until(b'\n', &mut drain).await;
            }
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32600, "message": "Request too large"}
            });
            let json = serialize_response(&response);
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            continue;
        }
        let line = match String::from_utf8(line_buf) {
            Ok(s) => s,
            Err(_) => {
                let response = Response::error(RequestId::Number(0), RpcError::parse_error());
                let json = serialize_response(&response);
                writer.write_all(json.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                continue;
            }
        };
        // The wait for a permit is unbounded, and shutdown can land inside
        // it. Taking work that queued before the signal and starting it after
        // is the same as never having seen the signal: by then the listener is
        // released and a replacement daemon owns the projects this request
        // would touch. Only a request already dispatched runs to its answer.
        let _permit = tokio::select! {
            biased;
            _ = shutdown_rx.wait_for(|stopping| *stopping) => break,
            permit = semaphore.acquire() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::debug!("Semaphore closed, ending connection");
                    break;
                }
            },
        };

        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => {
                let response = Response::error(RequestId::Number(0), RpcError::parse_error());
                let json = serialize_response(&response);
                writer.write_all(json.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                continue;
            }
        };

        let request_id = request.id.clone();
        let timeout = estimate_request_timeout(&request, &lsp_config);
        let result = tokio::time::timeout(
            timeout,
            process_request(request, &projects, &config, &lsp_config, start_time),
        )
        .await;

        let (response, should_shutdown) = match result {
            Ok(r) => r,
            Err(_) => (
                Response::error(request_id, RpcError::internal_error("Request timed out")),
                false,
            ),
        };

        let json = serialize_response(&response);

        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        if should_shutdown {
            shutdown.send_replace(true);
        }
    }

    Ok(())
}

fn estimate_request_timeout(request: &Request, lsp_config: &LspRuntimeConfig) -> Duration {
    use crate::models::symbol::Language;

    match methods::to_lsp_method(&request.method) {
        Some(lsp_method) => {
            let language = request
                .params
                .as_ref()
                .and_then(|p| p.get("file"))
                .and_then(|f| f.as_str())
                .map(|f| Language::from_path(Path::new(f)))
                .unwrap_or(Language::Unknown);
            lsp_config.timeout_for(language, lsp_method)
        }
        None => Duration::from_secs(600),
    }
}

async fn process_request(
    request: Request,
    projects: &ProjectsMap,
    config: &DaemonRuntimeConfig,
    lsp_config: &Arc<LspRuntimeConfig>,
    start_time: Instant,
) -> (Response, bool) {
    let id = request.id.clone();
    let is_shutdown = request.method == methods::SHUTDOWN;

    let result = dispatch(request, projects, config, lsp_config, start_time).await;
    let response = match result {
        Ok(v) => Response::success(id, v),
        Err(e) => Response::error(id, e),
    };

    (response, is_shutdown)
}
