//! MCP stdio server loop.
//!
//! Reads line-delimited JSON-RPC 2.0 messages from stdin and writes
//! responses to stdout. Notifications (no `id`) get no response. Requests
//! that fail tool dispatch return an MCP-style `isError: true` content
//! payload — not a JSON-RPC error — because that is what MCP clients
//! expect for tool execution failures.

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdin, Stdout};

use crate::app::App;

use super::protocol::{
    CallToolParams, Content, IncomingMessage, InitializeResult, ListToolsResult,
    MCP_PROTOCOL_VERSION, Response, RpcError, ServerCapabilities, ServerInfo, ToolsCapability,
};
use super::tools;

pub async fn serve_stdio(app: App) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    serve(stdin, stdout, app).await
}

async fn serve(stdin: Stdin, mut stdout: Stdout, app: App) -> Result<()> {
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = handle_line(&line, &app).await;
        if let Some(response_value) = response {
            let serialized = serde_json::to_string(&response_value)?;
            stdout.write_all(serialized.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

/// Handle one incoming JSON-RPC line. Returns `None` for notifications.
pub(super) async fn handle_line(line: &str, app: &App) -> Option<Value> {
    let message: IncomingMessage = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => {
            return serde_json::to_value(Response::failure(
                Value::Null,
                RpcError::parse_error(format!("Invalid JSON: {e}")),
            ))
            .ok();
        }
    };

    let id = message.id.clone();

    match message.method.as_str() {
        "initialize" => respond(id, handle_initialize()),
        "initialized" | "notifications/initialized" => None, // notification
        "ping" => respond(id, Ok(json!({}))),
        "shutdown" => respond(id, Ok(json!({}))),
        "tools/list" => respond(id, handle_tools_list()),
        "tools/call" => respond(id, handle_tools_call(message.params, app).await),
        other => respond(id, Err(RpcError::method_not_found(other))),
    }
}

fn respond(id: Option<Value>, result: Result<Value, RpcError>) -> Option<Value> {
    let id = id?; // notifications don't get a response
    let response = match result {
        Ok(v) => Response::success(id, v),
        Err(e) => Response::failure(id, e),
    };
    serde_json::to_value(response).ok()
}

fn handle_initialize() -> Result<Value, RpcError> {
    let result = InitializeResult {
        protocol_version: MCP_PROTOCOL_VERSION,
        capabilities: ServerCapabilities {
            tools: ToolsCapability {
                list_changed: false,
            },
        },
        server_info: ServerInfo {
            name: "symora",
            version: env!("CARGO_PKG_VERSION"),
        },
    };
    serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
}

fn handle_tools_list() -> Result<Value, RpcError> {
    let tools = tools::catalog().to_vec();
    serde_json::to_value(ListToolsResult { tools }).map_err(|e| RpcError::internal(e.to_string()))
}

async fn handle_tools_call(params: Option<Value>, app: &App) -> Result<Value, RpcError> {
    let params: CallToolParams = match params {
        Some(p) => serde_json::from_value(p)
            .map_err(|e| RpcError::invalid_params(format!("Invalid params: {e}")))?,
        None => return Err(RpcError::invalid_params("Missing params")),
    };

    match tools::dispatch(&params.name, params.arguments, app).await {
        Ok(content) => Ok(json!({ "content": content, "isError": false })),
        Err(e) => Ok(json!({
            "content": [Content::text(format!("Error: {e}"))],
            "isError": true,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"nonexistent"}"#,
            &dummy_app().await,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], RpcError::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notification_returns_no_response() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &dummy_app().await,
        )
        .await;
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn initialize_advertises_protocol_version() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &dummy_app().await,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(response["result"]["serverInfo"]["name"], "symora");
    }

    #[tokio::test]
    async fn tools_list_returns_catalog() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &dummy_app().await,
        )
        .await
        .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"get_project_overview"));
        assert!(names.contains(&"search_symbols"));
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let response = handle_line("{ not json", &dummy_app().await).await.unwrap();
        assert_eq!(response["error"]["code"], RpcError::PARSE_ERROR);
    }

    async fn dummy_app() -> App {
        // No daemon, no real LSP needed for the protocol-level tests above.
        App::new(crate::cli::OutputOptions::default(), false)
            .await
            .expect("app init")
    }
}
