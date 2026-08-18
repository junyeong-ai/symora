use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_repr::{Deserialize_repr, Serialize_repr};

// Re-export commonly used types from models
pub use crate::models::lsp::{Position, Range, TextEdit, WorkspaceEdit};

// JSON-RPC 2.0 Core Types

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
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl Response {
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    pub fn into_result(self) -> Result<Value, ResponseError> {
        match self.error {
            Some(err) => Err(err),
            None => Ok(self.result.unwrap_or(Value::Null)),
        }
    }
}

/// JSON-RPC 2.0 Notification (no id, no response expected)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
        }
    }
}

/// Request ID - can be number or string
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

impl From<u64> for RequestId {
    fn from(id: u64) -> Self {
        RequestId::Number(id)
    }
}

/// JSON-RPC 2.0 Error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for ResponseError {}

/// Standard JSON-RPC error codes
pub mod error_codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    // LSP-specific error codes
    pub const SERVER_NOT_INITIALIZED: i32 = -32002;
    pub const REQUEST_CANCELLED: i32 = -32800;
    pub const CONTENT_MODIFIED: i32 = -32801;

    // Symora-specific error codes
    pub const SERVER_TERMINATED: i32 = -32099;
}

/// Incoming message from LSP server
#[derive(Debug, Clone)]
pub enum Message {
    Response(Response),
    Request(Request),
    Notification(Notification),
}

impl Message {
    /// Parse a JSON string into a Message
    pub fn parse(json: &str) -> serde_json::Result<Self> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let has_id = value.get("id").is_some();
        let has_method = value.get("method").is_some();

        match (has_id, has_method) {
            (true, true) => Ok(Message::Request(serde_json::from_value(value)?)),
            (true, false) => Ok(Message::Response(serde_json::from_value(value)?)),
            (false, true) => Ok(Message::Notification(serde_json::from_value(value)?)),
            (false, false) => {
                use serde::de::Error;
                Err(serde_json::Error::custom("Invalid LSP message"))
            }
        }
    }
}

// LSP Initialize Types

/// Text document identifier
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

impl TextDocumentIdentifier {
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }
}

/// Text document position params
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentPositionParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
}

/// Client info for identification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Initialize params
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub process_id: Option<u32>,
    pub root_uri: Option<String>,
    /// Always populated alongside `root_uri`: clients that advertise
    /// `workspace.workspaceFolders` and then send `null` here push
    /// multi-root-aware servers (pyright) onto a nonexistent
    /// "<default workspace root>", disabling workspace-wide analysis.
    pub workspace_folders: Option<Vec<WorkspaceFolder>>,
    pub capabilities: ClientCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceFolder {
    pub uri: String,
    pub name: String,
}

/// Client capabilities
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub general: Option<GeneralClientCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowClientCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_document: Option<TextDocumentClientCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceClientCapabilities>,
    /// Server-specific extension capabilities (the LSP `experimental`
    /// bag). Servers ignore keys they don't know, so opting into one
    /// server's extension is safe to send everywhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,
}

/// General client capabilities (LSP 3.17+)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeneralClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_encodings: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_request_support: Option<StaleRequestSupport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regular_expressions: Option<RegularExpressionsCapability>,
}

/// Stale request support capability
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleRequestSupport {
    pub cancel: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_on_content_modified: Option<Vec<String>>,
}

/// Regular expression engine capability
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegularExpressionsCapability {
    pub engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Window client capabilities
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WindowClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_done_progress: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_message: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_document: Option<Value>,
}

/// Text document capabilities (LSP 3.17 complete)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synchronization: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hover: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_help: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_definition: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementation: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_highlight: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_symbol: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_action: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_lens: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_link: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatting: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_formatting: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_type_formatting: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish_diagnostics: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folding_range: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_range: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_editing_range: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_hierarchy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_tokens: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moniker: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_hierarchy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inlay_hint: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Value>,
}

/// Workspace capabilities (LSP 3.17 complete)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apply_edit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_edit: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub did_change_configuration: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub did_change_watched_files: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execute_command: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_folders: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_tokens: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_lens: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_operations: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inlay_hint: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Value>,
}

/// Server capabilities (from initialize response)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_document_sync: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hover_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_symbol_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_symbol_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_hierarchy_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_hierarchy_provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementation_provider: Option<Value>,
    /// The position encoding the server picked from the client's offered set
    /// (LSP 3.17). Absent means the spec default, utf-16.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_encoding: Option<String>,
}

/// The position encoding negotiated with a language server. Two-state by
/// design: `utf-16` is the LSP-mandatory floor, `utf-8` the preferred wire (its
/// `character` is already a byte offset). `utf-32` is never offered, so a
/// conformant server can never select it; were one to, a utf-32 character
/// equals a Unicode scalar and is served by the scalar path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEncoding {
    Utf8,
    #[default]
    Utf16,
}

impl PositionEncoding {
    /// The encoding the server echoed in its `InitializeResult`. Absent or
    /// unrecognized resolves to the LSP 3.17 default, utf-16 — a conformant
    /// path always exists, so an odd value never blocks a working flow.
    pub fn from_capabilities(caps: &ServerCapabilities) -> Self {
        match caps.position_encoding.as_deref() {
            Some("utf-8") => Self::Utf8,
            Some("utf-16") | None => Self::Utf16,
            Some(other) => {
                tracing::warn!(
                    "language server negotiated unsupported positionEncoding {other:?}; \
                     interpreting positions as utf-16"
                );
                Self::Utf16
            }
        }
    }
}

impl ServerCapabilities {
    /// Whether the server affirmatively advertises a provider for `feature`:
    /// the field is present and not `false`. Per LSP, an absent provider and
    /// `false` mean the same thing (no support), so this collapses to one
    /// binary signal — the only distinction symora acts on. Defined for the
    /// features whose handlers consult live capabilities (call hierarchy, type
    /// hierarchy, implementation); every other feature returns false.
    pub fn advertises(&self, feature: super::capabilities::LspFeature) -> bool {
        use super::capabilities::LspFeature;
        let provider = match feature {
            LspFeature::IncomingCalls | LspFeature::OutgoingCalls => &self.call_hierarchy_provider,
            LspFeature::TypeHierarchy => &self.type_hierarchy_provider,
            LspFeature::FindImplementations => &self.implementation_provider,
            _ => return false,
        };
        provider.as_ref().is_some_and(|v| *v != Value::Bool(false))
    }
}

/// Initialize result
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub capabilities: ServerCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_info: Option<ServerInfo>,
}

/// Server info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

// LSP Symbol Types

/// Symbol kind (LSP standard - integer values)
#[derive(Debug, Clone, Copy, Serialize_repr, Deserialize_repr, PartialEq, Eq)]
#[repr(u8)]
pub enum LspSymbolKind {
    File = 1,
    Module = 2,
    Namespace = 3,
    Package = 4,
    Class = 5,
    Method = 6,
    Property = 7,
    Field = 8,
    Constructor = 9,
    Enum = 10,
    Interface = 11,
    Function = 12,
    Variable = 13,
    Constant = 14,
    String = 15,
    Number = 16,
    Boolean = 17,
    Array = 18,
    Object = 19,
    Key = 20,
    Null = 21,
    EnumMember = 22,
    Struct = 23,
    Event = 24,
    Operator = 25,
    TypeParameter = 26,
}

/// Location in a document (LSP wire format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspLocation {
    pub uri: String,
    /// Range is required by LSP spec but some servers (kotlin-language-server) may omit it
    #[serde(default)]
    pub range: Range,
    /// The origin token the server identified at the query position — a
    /// `LocationLink`'s `originSelectionRange`, absent on plain `Location`
    /// responses. Wire coordinates in the QUERY file; carried so a reader
    /// can tell a definition of the token to itself from one that goes
    /// somewhere. Never (de)serialized — it exists only past parsing.
    #[serde(skip)]
    pub origin: Option<Range>,
}

/// LocationLink - used by some LSP servers (rust-analyzer) for definition responses
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationLink {
    /// The target resource identifier
    pub target_uri: String,
    /// The full target range
    pub target_range: Range,
    /// The span of the symbol at the target (used for highlighting)
    pub target_selection_range: Range,
    /// Span of the origin (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_selection_range: Option<Range>,
}

impl LocationLink {
    /// Convert LocationLink to LspLocation for uniform handling
    pub fn to_location(&self) -> LspLocation {
        LspLocation {
            uri: self.target_uri.clone(),
            range: self.target_selection_range.clone(),
            origin: self.origin_selection_range.clone(),
        }
    }
}

/// Document symbol
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbol {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub kind: LspSymbolKind,
    pub range: Range,
    pub selection_range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<DocumentSymbol>>,
}

/// Symbol information (workspace symbols)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInformation {
    pub name: String,
    pub kind: LspSymbolKind,
    pub location: LspLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
}

// LSP Hover Types

/// Hover result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hover {
    pub contents: HoverContents,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

/// Hover contents
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HoverContents {
    String(String),
    MarkupContent(MarkupContent),
    Array(Vec<MarkedString>),
}

/// Markup content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkupContent {
    pub kind: String,
    pub value: String,
}

/// Marked string
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MarkedString {
    String(String),
    LanguageString { language: String, value: String },
}

// LSP Diagnostic Types

/// Diagnostic severity (LSP standard - integer values)
#[derive(Debug, Clone, Copy, Serialize_repr, Deserialize_repr, PartialEq, Eq)]
#[repr(u8)]
pub enum LspDiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

/// Diagnostic tag (LSP standard - integer values)
#[derive(Debug, Clone, Copy, Serialize_repr, Deserialize_repr, PartialEq, Eq)]
#[repr(u8)]
pub enum LspDiagnosticTag {
    Unnecessary = 1,
    Deprecated = 2,
}

/// LSP Diagnostic
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDiagnostic {
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<LspDiagnosticSeverity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<LspDiagnosticTag>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_information: Vec<LspDiagnosticRelatedInformation>,
}

/// LSP Diagnostic Related Information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspDiagnosticRelatedInformation {
    pub location: LspLocation,
    pub message: String,
}

// LSP Call Hierarchy Types

/// Call hierarchy item
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspCallHierarchyItem {
    pub name: String,
    pub kind: LspSymbolKind,
    pub uri: String,
    pub range: Range,
    pub selection_range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Call hierarchy incoming call
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyIncomingCall {
    pub from: LspCallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

/// Call hierarchy outgoing call
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyOutgoingCall {
    pub to: LspCallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_is_present_and_not_false() {
        use crate::infra::lsp::capabilities::LspFeature;
        let with_ch = |v: Option<serde_json::Value>| ServerCapabilities {
            call_hierarchy_provider: v,
            ..Default::default()
        };
        // present-and-truthy: bool true and an options object both count.
        assert!(with_ch(Some(serde_json::json!(true))).advertises(LspFeature::IncomingCalls));
        assert!(with_ch(Some(serde_json::json!({}))).advertises(LspFeature::OutgoingCalls));
        // explicit false and absent both mean "not offered".
        assert!(!with_ch(Some(serde_json::json!(false))).advertises(LspFeature::IncomingCalls));
        assert!(!with_ch(None).advertises(LspFeature::IncomingCalls));

        // Each gated feature reads its own provider field.
        let impl_caps = ServerCapabilities {
            implementation_provider: Some(serde_json::json!(true)),
            ..Default::default()
        };
        assert!(impl_caps.advertises(LspFeature::FindImplementations));
        assert!(!impl_caps.advertises(LspFeature::TypeHierarchy));
        // A feature with no live-capability gate never reports advertised.
        assert!(!impl_caps.advertises(LspFeature::Hover));
    }

    #[test]
    fn position_encoding_omitted_or_unrecognized_defaults_to_utf16() {
        let caps = |enc: Option<&str>| ServerCapabilities {
            position_encoding: enc.map(String::from),
            ..Default::default()
        };
        assert_eq!(
            PositionEncoding::from_capabilities(&caps(None)),
            PositionEncoding::Utf16
        );
        assert_eq!(
            PositionEncoding::from_capabilities(&caps(Some("utf-16"))),
            PositionEncoding::Utf16
        );
        assert_eq!(
            PositionEncoding::from_capabilities(&caps(Some("utf-8"))),
            PositionEncoding::Utf8
        );
        // A non-conformant value never blocks: it is read as the utf-16 floor.
        assert_eq!(
            PositionEncoding::from_capabilities(&caps(Some("utf-7"))),
            PositionEncoding::Utf16
        );
    }

    #[test]
    fn test_request_serialization() {
        let req = Request::new(1, "initialize", Some(serde_json::json!({})));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"initialize\""));
    }

    #[test]
    fn test_response_parsing() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        assert!(resp.is_success());
        assert_eq!(resp.id, Some(RequestId::Number(1)));
    }

    #[test]
    fn test_error_response() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        assert!(!resp.is_success());
        assert!(resp.error.is_some());
    }
}
