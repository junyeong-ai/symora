//! MCP over HTTP — single-endpoint Streamable HTTP transport.
//!
//! POST `/` accepts a JSON-RPC 2.0 request body and returns the
//! corresponding JSON-RPC response (or an empty object for notifications).
//! Sessions, SSE upgrades, and server-initiated streams are intentionally
//! omitted: every Symora tool is single-shot, so the request/response
//! shape carries the full protocol surface clients need.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::app::App;

use super::McpProfile;
use super::server;

#[derive(Clone)]
struct HttpState {
    app: Arc<App>,
    profile: McpProfile,
}

pub async fn serve_http(app: App, addr: SocketAddr, profile: McpProfile) -> Result<()> {
    let state = HttpState {
        app: Arc::new(app),
        profile,
    };
    let router = Router::new()
        .route("/", post(handle_message))
        .with_state(state);

    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Symora MCP listening on http://{addr}");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("Symora MCP HTTP listener stopped cleanly");
    Ok(())
}

/// Future that resolves on the first OS-level termination signal.
/// `axum::serve(...).with_graceful_shutdown(...)` then stops accepting
/// new connections and waits for in-flight requests to finish.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return tokio::signal::ctrl_c().await.unwrap_or(()),
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn handle_message(
    State(state): State<HttpState>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let (status, body) = process_message(&state.app, state.profile, payload).await;
    (status, Json(body))
}

async fn process_message(app: &App, profile: McpProfile, payload: Value) -> (StatusCode, Value) {
    let line = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("Invalid JSON: {e}") }
                }),
            );
        }
    };

    match server::handle_line(&line, app, profile).await {
        Some(response) => (StatusCode::OK, response),
        // Notification: JSON-RPC says reply 202/empty, but 200 + `{}` is
        // friendlier to clients that don't special-case 202.
        None => (StatusCode::OK, serde_json::json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn dummy_app() -> App {
        App::new(crate::cli::OutputOptions::default(), false)
            .await
            .expect("app init")
    }

    #[tokio::test]
    async fn handler_returns_initialize_response() {
        let app = dummy_app().await;
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let (status, body) = process_message(&app, McpProfile::Full, payload).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["result"]["protocolVersion"],
            super::super::protocol::MCP_PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn handler_returns_empty_for_notification() {
        let app = dummy_app().await;
        let payload =
            serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let (status, body) = process_message(&app, McpProfile::Full, payload).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.as_object().unwrap().is_empty());
    }
}
