use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

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

    pub fn parse_error() -> Self {
        Self::new(-32700, "Parse error")
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
}

impl From<&crate::error::LspError> for RpcError {
    fn from(error: &crate::error::LspError) -> Self {
        let wire = super::wire_error::WireLspError::from(error);
        Self {
            code: error.error_code(),
            message: error.to_string(),
            data: serde_json::to_value(&wire).ok(),
        }
    }
}

impl From<crate::error::LspError> for RpcError {
    fn from(error: crate::error::LspError) -> Self {
        Self::from(&error)
    }
}

impl From<crate::error::StoreError> for RpcError {
    fn from(error: crate::error::StoreError) -> Self {
        use crate::error::StoreError;
        let code = match error {
            StoreError::NotInitialized => StoreError::NOT_INITIALIZED_CODE,
            StoreError::AlreadyIndexing => StoreError::ALREADY_INDEXING_CODE,
            StoreError::Busy => StoreError::BUSY_CODE,
            StoreError::Io(_) => StoreError::IO_CODE,
            StoreError::Rebuilding => StoreError::REBUILDING_CODE,
            StoreError::EmptyScope => StoreError::EMPTY_SCOPE_CODE,
            // No wire code of their own: a client reconstructs them as
            // `Database`, which renders identically because they share its
            // output code. Named rather than caught by a `_`, so a variant
            // added to `StoreError` fails to compile here until someone
            // decides which of the two it is.
            StoreError::Database(_)
            | StoreError::Corrupt(_)
            | StoreError::SchemaMismatch { .. } => -32603,
            // Only a client of a remote store can fail to reach it, and a
            // daemon owns its store outright — so this arm exists to fail
            // loudly in tests if that ever stops being true, rather than
            // shipping a transport error as a store error the client would
            // then reconstruct as one.
            StoreError::Unreachable(_) => {
                debug_assert!(false, "a daemon cannot fail to reach its own store");
                -32603
            }
        };
        // The variant's own message, unprefixed: the code names the domain,
        // and a caller reconstructing the error puts this straight back into
        // it, so any decoration here would compound on the way out.
        Self::new(code, error.to_string())
    }
}

impl From<serde_json::Error> for RpcError {
    fn from(error: serde_json::Error) -> Self {
        Self::internal_error(&error.to_string())
    }
}

/// Identity of what this process speaks: the crate's sources, its enabled
/// features, its target and profile, hashed at compile time (`build.rs`).
/// It is a compatibility token rather than an exact fingerprint of the
/// executable — a different toolchain compiling the same inputs produces
/// the same wire and the same identity, which is the property that matters.
/// The daemon reports it in its ping and a client accepts only its own, so
/// a daemon built from other inputs — including other inputs of the same
/// version — is replaced before any wire exchange rather than answering
/// with an incompatible payload.
pub const BUILD_ID: &str = env!("SYMORA_BUILD_ID");

pub mod methods {
    pub const FIND_SYMBOLS: &str = "find_symbols";
    pub const FIND_REFERENCES: &str = "find_references";
    pub const GOTO_DEFINITION: &str = "goto_definition";
    pub const GOTO_TYPE_DEFINITION: &str = "goto_type_definition";
    pub const FIND_IMPLEMENTATIONS: &str = "find_implementations";
    pub const WORKSPACE_SYMBOLS: &str = "workspace_symbols";
    pub const HOVER: &str = "hover";
    pub const SIGNATURE_HELP: &str = "signature_help";
    pub const DIAGNOSTICS: &str = "diagnostics";
    pub const INCOMING_CALLS: &str = "incoming_calls";
    pub const OUTGOING_CALLS: &str = "outgoing_calls";
    pub const SUPERTYPES: &str = "supertypes";
    pub const SUBTYPES: &str = "subtypes";
    pub const INLAY_HINTS: &str = "inlay_hints";
    pub const FOLDING_RANGES: &str = "folding_ranges";
    pub const SELECTION_RANGES: &str = "selection_ranges";
    pub const CODE_LENSES: &str = "code_lenses";
    pub const CODE_ACTIONS: &str = "code_actions";
    pub const APPLY_CODE_ACTION: &str = "apply_code_action";
    pub const PREPARE_RENAME: &str = "prepare_rename";
    pub const RENAME: &str = "rename";
    pub const PING: &str = "ping";
    pub const STATUS: &str = "status";
    pub const SHUTDOWN: &str = "shutdown";

    pub const REFRESH_FILES: &str = "refresh_files";
    pub const NOTE_FILES_EDITED: &str = "note_files_edited";
    pub const LANGUAGE_STATUS: &str = "language_status";

    pub const FORMAT: &str = "format";

    pub const SEARCH_SYMBOLS: &str = "search_symbols";
    pub const SEARCH_CONTENT: &str = "search_content";
    pub const INDEX_BUILD: &str = "index_build";
    pub const INDEX_STATUS: &str = "index_status";
    pub const INDEXED_LANGUAGES: &str = "indexed_languages";
    pub const INDEX_CLEAR: &str = "index_clear";

    pub fn to_lsp_method(daemon_method: &str) -> Option<&'static str> {
        match daemon_method {
            FIND_REFERENCES => Some("textDocument/references"),
            GOTO_DEFINITION => Some("textDocument/definition"),
            GOTO_TYPE_DEFINITION => Some("textDocument/typeDefinition"),
            FIND_IMPLEMENTATIONS => Some("textDocument/implementation"),
            FIND_SYMBOLS => Some("textDocument/documentSymbol"),
            WORKSPACE_SYMBOLS => Some("workspace/symbol"),
            HOVER => Some("textDocument/hover"),
            SIGNATURE_HELP => Some("textDocument/signatureHelp"),
            DIAGNOSTICS => Some("textDocument/publishDiagnostics"),
            INCOMING_CALLS => Some("callHierarchy/incomingCalls"),
            OUTGOING_CALLS => Some("callHierarchy/outgoingCalls"),
            SUPERTYPES => Some("typeHierarchy/supertypes"),
            SUBTYPES => Some("typeHierarchy/subtypes"),
            INLAY_HINTS => Some("textDocument/inlayHint"),
            FOLDING_RANGES => Some("textDocument/foldingRange"),
            SELECTION_RANGES => Some("textDocument/selectionRange"),
            CODE_LENSES => Some("textDocument/codeLens"),
            CODE_ACTIONS => Some("textDocument/codeAction"),
            APPLY_CODE_ACTION => Some("codeAction/resolve"),
            PREPARE_RENAME => Some("textDocument/prepareRename"),
            RENAME => Some("textDocument/rename"),
            FORMAT => Some("textDocument/formatting"),
            PING | STATUS | SHUTDOWN | REFRESH_FILES | NOTE_FILES_EDITED | LANGUAGE_STATUS
            | INDEX_BUILD | INDEX_STATUS | INDEX_CLEAR | SEARCH_SYMBOLS | SEARCH_CONTENT => None,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let req = Request::new(
            1,
            methods::FIND_SYMBOLS,
            Some(serde_json::json!({"file": "test.rs"})),
        );
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"find_symbols\""));
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
