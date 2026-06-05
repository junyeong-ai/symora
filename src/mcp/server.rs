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

use super::McpProfile;
use super::instructions::INSTRUCTIONS;
use super::protocol::{
    CallToolParams, Content, IncomingMessage, InitializeResult, ListToolsResult, Response,
    RpcError, ServerCapabilities, ServerInfo, ToolsCapability, negotiate_protocol_version,
};
use super::tools;

pub async fn serve_stdio(app: App, profile: McpProfile) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    serve(stdin, stdout, app, profile).await
}

async fn serve(stdin: Stdin, mut stdout: Stdout, app: App, profile: McpProfile) -> Result<()> {
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = handle_line(&line, &app, profile).await;
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
pub(super) async fn handle_line(line: &str, app: &App, profile: McpProfile) -> Option<Value> {
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
        "initialize" => respond(id, handle_initialize(message.params.as_ref())),
        "initialized" | "notifications/initialized" => None, // notification
        "ping" => respond(id, Ok(json!({}))),
        "shutdown" => respond(id, Ok(json!({}))),
        "tools/list" => respond(id, handle_tools_list(profile)),
        "tools/call" => respond(id, handle_tools_call(message.params, app, profile).await),
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

fn handle_initialize(params: Option<&Value>) -> Result<Value, RpcError> {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str());
    let result = InitializeResult {
        protocol_version: negotiate_protocol_version(requested),
        capabilities: ServerCapabilities {
            tools: ToolsCapability {
                list_changed: false,
            },
        },
        server_info: ServerInfo {
            name: "symora",
            version: env!("CARGO_PKG_VERSION"),
        },
        instructions: INSTRUCTIONS,
    };
    serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
}

fn handle_tools_list(profile: McpProfile) -> Result<Value, RpcError> {
    let tools = tools::visible_catalog(profile);
    serde_json::to_value(ListToolsResult { tools }).map_err(|e| RpcError::internal(e.to_string()))
}

async fn handle_tools_call(
    params: Option<Value>,
    app: &App,
    profile: McpProfile,
) -> Result<Value, RpcError> {
    let params: CallToolParams = match params {
        Some(p) => serde_json::from_value(p)
            .map_err(|e| RpcError::invalid_params(format!("Invalid params: {e}")))?,
        None => return Err(RpcError::invalid_params("Missing params")),
    };

    match tools::dispatch(&params.name, params.arguments, app, profile).await {
        Ok(output) => {
            let mut result = json!({ "content": output.content, "isError": output.is_error });
            if let Some(structured) = output.structured {
                result["structuredContent"] = structured;
            }
            Ok(result)
        }
        // Same `{"error": {code, message, hint}}` body the CLI emits —
        // one error vocabulary across both surfaces.
        Err(e) => {
            let body = json!({ "error": e });
            Ok(json!({
                "content": [Content::text(body.to_string())],
                "structuredContent": body,
                "isError": true,
            }))
        }
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
            McpProfile::Full,
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
            McpProfile::Full,
        )
        .await;
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn initialize_advertises_protocol_version() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        assert_eq!(
            response["result"]["protocolVersion"],
            super::super::protocol::MCP_PROTOCOL_VERSION
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "symora");
    }

    #[tokio::test]
    async fn initialize_echoes_a_supported_client_version() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
    }

    #[tokio::test]
    async fn initialize_answers_unknown_version_with_newest_supported() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        assert_eq!(
            response["result"]["protocolVersion"],
            super::super::protocol::MCP_PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn list_tools_advertise_output_schema_for_list_shapes() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        let search = tools
            .iter()
            .find(|t| t["name"] == "search_symbols")
            .unwrap();
        let props = &search["outputSchema"]["properties"];
        for key in ["count", "showing", "items", "truncated", "hints", "error"] {
            assert!(props.get(key).is_some(), "outputSchema missing {key}");
        }
    }

    #[tokio::test]
    async fn tools_call_returns_structured_content_alongside_text() {
        // Handled error path is hermetic and emits a single JSON object.
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_symbols","arguments":{"query":""}}}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        let text: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(response["result"]["structuredContent"], text);
    }

    #[tokio::test]
    async fn tools_list_returns_catalog() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &dummy_app().await,
            McpProfile::Full,
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
    async fn unknown_tool_returns_structured_not_found() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        let body: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn invalid_tool_arguments_return_structured_invalid_argument() {
        // `search_symbols` requires a string `query`.
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_symbols","arguments":{"query":42}}}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        let body: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "invalid_argument");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Invalid tool arguments")
        );
    }

    #[tokio::test]
    async fn handled_command_error_sets_is_error_with_structured_body() {
        // Empty query is a handled failure: the command prints a structured
        // error and returns Ok — `isError` must still be true.
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_symbols","arguments":{"query":""}}}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        let body: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "invalid_argument");
        assert!(body["error"]["message"].as_str().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn initialize_carries_the_instructions_playbook() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        let instructions = response["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("search_symbols"));
    }

    #[tokio::test]
    async fn full_profile_lists_annotations_on_every_tool() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &dummy_app().await,
            McpProfile::Full,
        )
        .await
        .unwrap();
        for tool in response["result"]["tools"].as_array().unwrap() {
            assert!(
                tool["annotations"]["readOnlyHint"].is_boolean(),
                "{} missing readOnlyHint",
                tool["name"]
            );
        }
    }

    #[tokio::test]
    async fn read_only_profile_hides_mutating_tools_from_list() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &dummy_app().await,
            McpProfile::ReadOnly,
        )
        .await
        .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        for tool in tools {
            assert_eq!(
                tool["annotations"]["readOnlyHint"], true,
                "{} leaked into the read-only profile",
                tool["name"]
            );
        }
    }

    #[tokio::test]
    async fn read_only_profile_refuses_mutating_calls() {
        // Hiding a tool from tools/list without refusing tools/call would
        // be cosmetic, not a boundary.
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"replace_symbol_body","arguments":{"file":"src/main.rs","line":1,"body":"x"}}}"#,
            &dummy_app().await,
            McpProfile::ReadOnly,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        let body: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "unsupported");
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let response = handle_line("{ not json", &dummy_app().await, McpProfile::Full)
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], RpcError::PARSE_ERROR);
    }

    async fn dummy_app() -> App {
        // No daemon, no real LSP needed for the protocol-level tests above.
        App::new(crate::cli::OutputOptions::default(), false)
            .await
            .expect("app init")
    }
}
