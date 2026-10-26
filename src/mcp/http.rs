use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::net::TcpListener;

use super::{McpProfile, server::McpServer};
use crate::app::App;

pub async fn serve_http(app: App, addr: SocketAddr, profile: McpProfile) -> Result<()> {
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "MCP HTTP binds only to loopback; expose it through an authenticated proxy for remote access"
    );
    let server = McpServer::new(Arc::new(app), profile);
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    config.json_response = true;
    config.allowed_origins = [
        "http://localhost:*",
        "https://localhost:*",
        "http://127.0.0.1:*",
        "https://127.0.0.1:*",
        "http://[::1]:*",
        "https://[::1]:*",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let cancellation = config.cancellation_token.clone();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let router = Router::new().route_service("/", service);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Symora MCP listening on http://{addr}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            crate::infra::signal::stop_requested().await;
            cancellation.cancel();
        })
        .await?;
    Ok(())
}
