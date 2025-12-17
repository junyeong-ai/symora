//! JSON-RPC 2.0 Protocol for Daemon Communication
//!
//! Defines the protocol for CLI-Daemon communication over Unix socket.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 Request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(id),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 Response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: RequestId, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// Request ID
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

/// JSON-RPC Error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn parse_error() -> Self {
        Self::new(-32700, "Parse error")
    }

    pub fn invalid_request() -> Self {
        Self::new(-32600, "Invalid request")
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("Method not found: {}", method))
    }

    pub fn invalid_params(msg: &str) -> Self {
        Self::new(-32602, format!("Invalid params: {}", msg))
    }

    pub fn internal_error(msg: &str) -> Self {
        Self::new(-32603, format!("Internal error: {}", msg))
    }

    pub fn from_lsp_error(error: &crate::error::LspError) -> Self {
        Self::new(error.error_code(), error.to_string())
    }

    pub fn server_not_installed(server: &str, hint: &str) -> Self {
        Self::with_data(
            -32001,
            format!("Server not installed: {}", server),
            serde_json::json!({
                "server": server,
                "install_hint": hint
            }),
        )
    }
}

impl From<&crate::error::LspError> for RpcError {
    fn from(error: &crate::error::LspError) -> Self {
        Self::new(error.error_code(), error.to_string())
    }
}

impl From<crate::error::LspError> for RpcError {
    fn from(error: crate::error::LspError) -> Self {
        Self::from(&error)
    }
}

/// RPC method constants
pub mod methods {
    pub const FIND_SYMBOL: &str = "find_symbol";
    pub const FIND_REFS: &str = "find_refs";
    pub const FIND_DEF: &str = "find_def";
    pub const FIND_TYPEDEF: &str = "find_typedef";
    pub const FIND_IMPL: &str = "find_impl";
    pub const WORKSPACE_SYMBOL: &str = "workspace_symbol";
    pub const HOVER: &str = "hover";
    pub const SIGNATURE_HELP: &str = "signature_help";
    pub const DIAGNOSTICS: &str = "diagnostics";
    pub const CALLS_INCOMING: &str = "calls_incoming";
    pub const CALLS_OUTGOING: &str = "calls_outgoing";
    pub const SUPERTYPES: &str = "supertypes";
    pub const SUBTYPES: &str = "subtypes";
    pub const INLAY_HINTS: &str = "inlay_hints";
    pub const FOLDING_RANGES: &str = "folding_ranges";
    pub const SELECTION_RANGES: &str = "selection_ranges";
    pub const CODE_LENS: &str = "code_lens";
    pub const CODE_ACTIONS: &str = "code_actions";
    pub const APPLY_CODE_ACTION: &str = "apply_code_action";
    pub const PREPARE_RENAME: &str = "prepare_rename";
    pub const RENAME: &str = "rename";
    pub const PING: &str = "ping";
    pub const STATUS: &str = "status";
    pub const SHUTDOWN: &str = "shutdown";
}

/// Client-side response DTOs for deserialization
pub mod dto {
    use serde::Deserialize;

    use crate::daemon::dto::{CallItemDto, DiagnosticDto, LocationDto, SignatureDto, SymbolDto};

    // Re-export shared DTOs from daemon::dto
    pub use crate::daemon::dto::{HoverResponse, RangeDto, ReferencesResponse};

    #[derive(Debug, Deserialize)]
    pub struct SymbolsResponse {
        pub count: usize,
        pub symbols: Vec<SymbolDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct CallsResponse {
        pub count: usize,
        pub calls: Vec<CallItemDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct DefinitionResponse {
        pub definition: Option<LocationDto>,
        #[serde(default)]
        pub message: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct ImplementationsResponse {
        pub count: usize,
        pub implementations: Vec<LocationDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct SignatureResponse {
        pub signatures: Vec<SignatureDto>,
        pub active_signature: Option<u32>,
        pub active_parameter: Option<u32>,
        #[serde(default)]
        pub message: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct DiagnosticsResponse {
        pub count: usize,
        pub diagnostics: Vec<DiagnosticDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct PrepareRenameResponse {
        pub placeholder: Option<String>,
        pub range: Option<RangeDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct RenameResponse {
        pub changes: Vec<FileChangeSummary>,
    }

    #[derive(Debug, Deserialize)]
    pub struct FileChangeSummary {
        pub file: String,
        pub edit_count: usize,
    }

    #[derive(Debug, Deserialize)]
    pub struct CodeActionsResponse {
        pub count: usize,
        pub actions: Vec<CodeActionDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct CodeActionDto {
        pub title: String,
        pub kind: Option<String>,
        pub is_preferred: bool,
        pub diagnostics: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct ApplyActionResponse {
        pub changes: Vec<FileEditDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct FileEditDto {
        pub file: String,
        pub edits: Vec<TextEditDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct TextEditDto {
        pub range: RangeDto,
        pub new_text: String,
    }

    #[derive(Debug, Deserialize)]
    pub struct TypeHierarchyResponse {
        pub count: usize,
        pub items: Vec<TypeHierarchyItemDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct TypeHierarchyItemDto {
        pub name: String,
        pub kind: String,
        pub file: String,
        pub line: u32,
        pub column: u32,
        pub detail: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct InlayHintsResponse {
        pub count: usize,
        pub hints: Vec<InlayHintDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct InlayHintDto {
        pub line: u32,
        pub character: u32,
        pub label: String,
        pub kind: Option<u32>,
        #[serde(default)]
        pub padding_left: bool,
        #[serde(default)]
        pub padding_right: bool,
    }

    #[derive(Debug, Deserialize)]
    pub struct FoldingRangesResponse {
        pub count: usize,
        pub ranges: Vec<FoldingRangeDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct FoldingRangeDto {
        pub start_line: u32,
        pub end_line: u32,
        pub start_character: Option<u32>,
        pub end_character: Option<u32>,
        pub kind: Option<String>,
        pub collapsed_text: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct SelectionRangesResponse {
        pub count: usize,
        pub ranges: Vec<SelectionRangeDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct SelectionRangeDto {
        pub start_line: u32,
        pub start_character: u32,
        pub end_line: u32,
        pub end_character: u32,
        pub parent: Option<Box<SelectionRangeDto>>,
    }

    #[derive(Debug, Deserialize)]
    pub struct CodeLensResponse {
        pub count: usize,
        pub lenses: Vec<CodeLensDto>,
    }

    #[derive(Debug, Deserialize)]
    pub struct CodeLensDto {
        pub start_line: u32,
        pub start_character: u32,
        pub end_line: u32,
        pub end_character: u32,
        pub command: Option<CodeLensCommandDto>,
        pub data: Option<serde_json::Value>,
    }

    #[derive(Debug, Deserialize)]
    pub struct CodeLensCommandDto {
        pub title: String,
        pub command: String,
        #[serde(default)]
        pub arguments: Vec<serde_json::Value>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let req = Request::new(
            1,
            "find_symbol",
            Some(serde_json::json!({"file": "test.rs"})),
        );
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"find_symbol\""));
    }

    #[test]
    fn test_response_success() {
        let resp = Response::success(RequestId::Number(1), serde_json::json!({"count": 5}));
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
    }

    #[test]
    fn test_response_error() {
        let resp = Response::error(RequestId::Number(1), RpcError::method_not_found("unknown"));
        assert!(resp.error.is_some());
        assert!(resp.result.is_none());
    }
}
