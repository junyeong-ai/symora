use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, watch};

use serde::Serialize;

use crate::daemon::protocol::{Request, RequestId, Response, RpcError, methods};

use super::Instance;
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
    instance: Instance,
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
                return Err(crate::error::LspError::timeout(
                    "Caller time budget expired before dispatch".to_string(),
                ));
            }
            Ok(Some(
                process_request(request, &projects, &config, &instance).await,
            ))
        };
        let processed = match allowance {
            Some(allowance) => crate::infra::deadline::run(allowance, process).await,
            None => process.await,
        }
        .unwrap_or_else(|error| Some((Response::error(id, RpcError::from(error)), false)));
        let Some((mut response, should_shutdown)) = processed else {
            break;
        };
        response.instance = Some(instance.id.to_string());

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
    instance: &Instance,
) -> (Response, bool) {
    let id = request.id.clone();
    let is_shutdown = request.method == methods::SHUTDOWN;

    let result = dispatch(request, projects, config, instance).await;
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
            Instance::new(),
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
            crate::error::LspError::timeout(String::new()).error_code()
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

    /// Every answer names the instance that gave it, and a request bound to
    /// another instance is refused, typed, before any of its work: no project
    /// is loaded for it. One bound to this instance is served.
    #[tokio::test]
    async fn a_request_bound_to_another_instance_is_refused_before_its_work() {
        let root = tempfile::tempdir().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let config = Arc::new(DaemonRuntimeConfig {
            socket_path: root.path().join("daemon.sock"),
            pid_path: root.path().join("daemon.pid"),
            lock_path: root.path().join("daemon.lock"),
            bind_lock_path: root.path().join("bind.lock"),
            idle_timeout: Duration::from_secs(60),
            max_concurrent: 1,
        });
        let instance = Instance::new();
        let (shutdown, _) = watch::channel(false);
        let (client, server) = UnixStream::pair().unwrap();
        let handler = tokio::spawn(handle_connection(
            server,
            Arc::clone(&projects),
            Arc::new(Semaphore::new(1)),
            config,
            instance.clone(),
            shutdown,
        ));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        let mut exchange = async |request: Request| {
            writer
                .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            serde_json::from_str::<Response>(&line).unwrap()
        };

        let mut hover = Request::new(
            1,
            methods::HOVER,
            Some(serde_json::json!({
                "project": root.path(), "file": "main.go", "line": 1, "column": 1
            })),
        );
        hover.instance = Some(format!("not-{}", instance.id));
        let refused = exchange(hover).await;
        assert_eq!(refused.instance.as_deref(), Some(&*instance.id));
        let wire: crate::daemon::wire_error::WireLspError =
            serde_json::from_value(refused.error.unwrap().data.unwrap()).unwrap();
        assert!(
            matches!(
                crate::error::LspError::from(wire),
                crate::error::LspError::DaemonReplaced(ref method) if method == methods::HOVER
            ),
            "the refusal is typed"
        );
        assert!(projects.read().await.is_empty(), "no project was loaded");

        let mut ping = Request::new(2, methods::PING, None);
        ping.instance = Some(instance.id.to_string());
        let served = exchange(ping).await;
        assert_eq!(served.result.unwrap()["pong"], true);
        assert_eq!(served.instance.as_deref(), Some(&*instance.id));
        drop(exchange);
        drop(writer);
        drop(reader);
        handler.await.unwrap().unwrap();
    }
}
