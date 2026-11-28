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
    mut abandon: watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    const MAX_LINE_SIZE: usize = 10 * 1024 * 1024; // 10MB

    // Shutdown releases the listener, so no NEW connection reaches this
    // process — but a connection already accepted would keep serving
    // requests from it, and a replacement is serving the socket by then.
    // Two daemons answering for the same projects is the state the socket
    // claim exists to prevent, so an established connection stops taking
    // work at the same moment a new one stops arriving: a request waiting for
    // a permit is answered `DaemonStopping`, and the next is not read. One
    // already dispatched still gets its answer, which the daemon waits for
    // before it stops the servers it asks (`DaemonServer::drain`), unless the
    // drain's bound comes first (`abandon`): it is answered `DaemonStopping`.
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
        let method = request.method.clone();
        let allowance = request.timeout_ms.map(|milliseconds| {
            std::time::Duration::from_millis(milliseconds).saturating_sub(received.elapsed())
        });
        let expires_at = allowance.and_then(crate::infra::deadline::expires_at);
        let process = async {
            let _permit = tokio::select! {
                biased;
                _ = shutdown_rx.wait_for(|stopping| *stopping) => {
                    return Err(crate::error::LspError::DaemonStopping(method.clone()));
                }
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
        let work = async {
            match allowance {
                Some(allowance) => crate::infra::deadline::run(allowance, process).await,
                None => process.await,
            }
        };
        let processed = tokio::select! {
            biased;
            processed = work => processed,
            Ok(_) = abandon.wait_for(|given_up| *given_up) => {
                Err(crate::error::LspError::DaemonStopping(method.clone()))
            }
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
            watch::channel(false).1,
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
            watch::channel(false).1,
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

    fn test_config(root: &std::path::Path) -> Arc<DaemonRuntimeConfig> {
        Arc::new(DaemonRuntimeConfig {
            socket_path: root.join("daemon.sock"),
            pid_path: root.join("daemon.pid"),
            lock_path: root.join("daemon.lock"),
            bind_lock_path: root.join("bind.lock"),
            idle_timeout: Duration::from_secs(60),
            max_concurrent: 1,
        })
    }

    fn stopping_method(response: Response) -> Option<String> {
        let wire: crate::daemon::wire_error::WireLspError =
            serde_json::from_value(response.error?.data?).ok()?;
        match crate::error::LspError::from(wire) {
            crate::error::LspError::DaemonStopping(method) => Some(method),
            _ => None,
        }
    }

    /// A request still waiting for a permit when its daemon stops is answered
    /// that the daemon stopped, typed, before any of its work, and the
    /// connection takes no further request.
    #[tokio::test(start_paused = true)]
    async fn a_request_queued_when_its_daemon_stops_is_answered_as_stopping() {
        let root = tempfile::tempdir().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let (shutdown, _) = watch::channel(false);
        let (client, server) = UnixStream::pair().unwrap();
        let handler = tokio::spawn(handle_connection(
            server,
            Arc::clone(&projects),
            Arc::new(Semaphore::new(0)),
            test_config(root.path()),
            Instance::new(),
            shutdown.clone(),
            watch::channel(false).1,
        ));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        let hover = Request::new(
            1,
            methods::HOVER,
            Some(serde_json::json!({
                "project": root.path(), "file": "main.go", "line": 1, "column": 1
            })),
        );
        writer
            .write_all(format!("{}\n", serde_json::to_string(&hover).unwrap()).as_bytes())
            .await
            .unwrap();
        // The clock is paused, so it moves only once nothing else can run:
        // by then the handler has read the request and waits for a permit.
        tokio::time::sleep(Duration::from_millis(1)).await;
        shutdown.send_replace(true);

        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("the queued request was never answered")
            .unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(response.id, RequestId::Number(1));
        assert_eq!(stopping_method(response).as_deref(), Some(methods::HOVER));
        assert!(projects.read().await.is_empty(), "no project was loaded");
        handler.await.unwrap().unwrap();
    }

    /// A request in progress when its daemon stops is left to finish, its
    /// server left running, until the drain gives it up: only then is it
    /// answered that the daemon stopped.
    #[tokio::test]
    async fn a_request_in_progress_is_answered_as_stopping_once_the_drain_gives_it_up() {
        use crate::infra::lsp::manager::tests::server_lifetime::{FakeServer, START_BUDGET_SECS};
        use crate::infra::lsp::watch::FileWatch;
        use crate::services::lsp::DefaultLspService;

        let fake = FakeServer::new();
        let root = fake.dir.path();
        std::fs::write(root.join("main.go"), "package main\n").unwrap();
        let mut config = crate::models::config::SymoraConfig::default();
        config.lsp.timeout_secs = START_BUDGET_SECS;
        // The fake publishes nothing, so the read waits for as long as its
        // budget lets it.
        config.lsp.diagnostics_wait_ms = 2 * START_BUDGET_SECS * 1000;
        config
            .lsp
            .servers
            .insert("go".to_string(), fake.server("serve", 0));
        let mut ctx = super::super::context::ProjectContext::new(root);
        ctx.lsp = Arc::new(DefaultLspService::new(
            root,
            Arc::new(crate::config::LspRuntimeConfig::from(&config)),
            FileWatch::Off,
        ));
        let ctx = Arc::new(ctx);
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::from([(
            root.to_path_buf(),
            Arc::clone(&ctx),
        )])));
        let (shutdown, _) = watch::channel(false);
        let (abandon, given_up) = watch::channel(false);
        let (client, server) = UnixStream::pair().unwrap();
        let handler = tokio::spawn(handle_connection(
            server,
            Arc::clone(&projects),
            Arc::new(Semaphore::new(1)),
            test_config(root),
            Instance::new(),
            shutdown.clone(),
            given_up,
        ));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        let diagnostics = Request::new(
            1,
            methods::DIAGNOSTICS,
            Some(serde_json::json!({"project": root, "file": root.join("main.go")})),
        );
        writer
            .write_all(format!("{}\n", serde_json::to_string(&diagnostics).unwrap()).as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2 * START_BUDGET_SECS), async {
            while !fake.serving() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the request never reached its server");

        shutdown.send_replace(true);
        let mut line = String::new();
        let early = tokio::time::timeout(Duration::from_millis(300), reader.read_line(&mut line));
        assert!(early.await.is_err(), "answered while draining: {line}");
        assert!(fake.serving(), "the server was stopped under the request");

        abandon.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("the request was never answered")
            .unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(response.id, RequestId::Number(1));
        assert_eq!(
            stopping_method(response).as_deref(),
            Some(methods::DIAGNOSTICS)
        );
        handler.await.unwrap().unwrap();
        ctx.lsp.shutdown().await;
        fake.assert_all_gone().await;
    }
}
