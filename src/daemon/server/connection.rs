use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, watch};

use serde::Serialize;

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
        let received = tokio::time::Instant::now();
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

        let id = request.id.clone();
        let allowance = request.timeout_ms.map(|milliseconds| {
            std::time::Duration::from_millis(milliseconds).saturating_sub(received.elapsed())
        });
        let expires_at = allowance.and_then(crate::infra::deadline::expires_at);
        let process = async {
            let _permit = tokio::select! {
                biased;
                _ = shutdown_rx.wait_for(|stopping| *stopping) => return Ok(None),
                permit = semaphore.acquire() => match permit {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::debug!("Semaphore closed, ending connection");
                        return Ok(None);
                    }
                },
            };
            if expires_at.is_some_and(|end| end <= tokio::time::Instant::now()) {
                return Err(crate::error::LspError::Timeout(
                    "Caller time budget expired before dispatch".to_string(),
                ));
            }
            Ok(Some(
                process_request(request, &projects, &config, start_time).await,
            ))
        };
        let processed = match allowance {
            Some(allowance) => crate::infra::deadline::run(allowance, process).await,
            None => process.await,
        }
        .unwrap_or_else(|error| Some((Response::error(id, RpcError::from(error)), false)));
        let Some((response, should_shutdown)) = processed else {
            break;
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

async fn process_request(
    request: Request,
    projects: &ProjectsMap,
    config: &DaemonRuntimeConfig,
    start_time: Instant,
) -> (Response, bool) {
    let id = request.id.clone();
    let is_shutdown = request.method == methods::SHUTDOWN;

    let result = dispatch(request, projects, config, start_time).await;
    let response = match result {
        Ok(v) => Response::success(id, v),
        Err(e) => Response::error(id, e),
    };

    (response, is_shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::sync::RwLock;

    #[tokio::test]
    async fn a_queued_request_expires_without_dispatch_and_the_connection_remains_usable() {
        let root = tempfile::tempdir().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let semaphore = Arc::new(Semaphore::new(0));
        let config = Arc::new(DaemonRuntimeConfig {
            socket_path: root.path().join("daemon.sock"),
            pid_path: root.path().join("daemon.pid"),
            lock_path: root.path().join("daemon.lock"),
            bind_lock_path: root.path().join("bind.lock"),
            idle_timeout: Duration::from_secs(60),
            max_concurrent: 1,
        });
        let (shutdown, _) = watch::channel(false);
        let (client, server) = UnixStream::pair().unwrap();
        let handler = tokio::spawn(handle_connection(
            server,
            Arc::clone(&projects),
            Arc::clone(&semaphore),
            config,
            Instant::now(),
            shutdown,
        ));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        let mut request = Request::new(
            1,
            methods::HOVER,
            Some(serde_json::json!({
                "project": root.path(), "file": "main.go", "line": 1, "column": 1
            })),
        );
        request.timeout_ms = Some(20);
        writer
            .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("queued request must time out before a permit is released")
            .unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(response.id, RequestId::Number(1));
        assert_eq!(
            response.error.unwrap().code,
            crate::error::LspError::Timeout(String::new()).error_code()
        );
        assert!(projects.read().await.is_empty());

        semaphore.add_permits(1);
        let ping = Request::new(2, methods::PING, None);
        writer
            .write_all(format!("{}\n", serde_json::to_string(&ping).unwrap()).as_bytes())
            .await
            .unwrap();
        line.clear();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(response.id, RequestId::Number(2));
        assert_eq!(response.result.unwrap()["pong"], true);
        drop(writer);
        drop(reader);
        handler.await.unwrap().unwrap();
    }
}
