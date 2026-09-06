use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, LspError, ProjectError, SearchError, StoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    Unsupported,
    Timeout,
    InvalidArgument,
    Internal,
    LspUnavailable,
    LanguageNotConfigured,
    ServerNotInstalled,
    Cancelled,
    ParseError,
    StoreNotInitialized,
    AlreadyExists,
    /// What the command needs moved or is held by someone else: an edit
    /// range that no longer matches the on-disk file (the analysis ran
    /// against a stale revision), or an index another process is rebuilding.
    /// Agents branch on this to retry — re-reading first, where the state
    /// is a file revision — rather than treating it as a generic internal
    /// failure.
    Conflict,
    /// An asserted precondition on a mutating command is unmet or could not
    /// be verified (e.g. `--expect-no-references` on `edit delete`). Unlike
    /// `Conflict`, re-reading and retrying will not clear it: the agent must
    /// change the underlying state (fix the references), wait out the named
    /// degradation, or drop the assertion.
    PreconditionFailed,
    FileTooLarge,
    Io,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OutputError {}

impl OutputError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }

    pub fn precondition_failed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PreconditionFailed, message)
    }

    pub fn lsp_unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::LspUnavailable, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Timeout, message)
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ParseError, message)
    }
}

impl From<LspError> for OutputError {
    fn from(err: LspError) -> Self {
        let message = err.to_string();
        match err {
            LspError::ServerNotInstalled { install_hint, .. } => {
                Self::new(ErrorCode::ServerNotInstalled, message).with_hint(install_hint)
            }
            LspError::UnsupportedLanguage(_) => {
                Self::new(ErrorCode::LanguageNotConfigured, message)
                    .with_hint("Run 'symora doctor' to see supported languages.")
            }
            LspError::FeatureNotSupported { suggestion, .. } => {
                Self::new(ErrorCode::Unsupported, message).with_hint(suggestion)
            }
            LspError::ServerTerminated { established, .. } => {
                Self::new(ErrorCode::LspUnavailable, message).with_hint(match established {
                    true => "The session dropped — retry, or `symora daemon restart`.".to_string(),
                    false => "The server never answered initialize, so restarting repeats it — \
                              run `symora doctor` for what it does on this workspace. \
                              `symora symbols`, `symora search` and `symora map` answer without \
                              a language server."
                        .to_string(),
                })
            }
            LspError::ServerStart(_) => Self::new(ErrorCode::LspUnavailable, message).with_hint(
                "The server reported why above — resolve that, then retry. \
                 `symora search content`, `symora search ast`, and `symora map` \
                 answer without a language server.",
            ),
            LspError::NotConnected => {
                Self::new(ErrorCode::LspUnavailable, message).with_hint("Try: symora daemon start")
            }
            // The answer was lost, not refused: the same request against the
            // daemon that replaced this one succeeds.
            LspError::ConnectionLost(_) => Self::new(ErrorCode::LspUnavailable, message)
                .with_hint("The daemon was stopping or being replaced — retry."),
            LspError::Timeout(_) => Self::new(ErrorCode::Timeout, message),
            // Same recovery contract as a timeout — the answer arrives once
            // the server warms up — so it shares the retryable code.
            LspError::Indexing { .. } => Self::new(ErrorCode::Timeout, message)
                .with_hint("The language server is still indexing; retry shortly."),
            LspError::RequestCancelled => Self::new(ErrorCode::Cancelled, message),
            LspError::FileTooLarge { .. } => Self::new(ErrorCode::FileTooLarge, message),
            LspError::FileNotText { .. } => Self::new(ErrorCode::Unsupported, message),
            LspError::ServerError { code, message: m } => classify_server_error(code, &m, message),
            LspError::Protocol(_) => Self::new(ErrorCode::Internal, message),
            LspError::UnsupportedEdit(_) => Self::new(ErrorCode::Unsupported, message),
            LspError::Io(_) => Self::new(ErrorCode::Io, message),
            LspError::Json(_) => Self::new(ErrorCode::ParseError, message),
        }
    }
}

/// Classify a JSON-RPC `ServerError` into the most useful `ErrorCode` we
/// can derive from the standard code + the message body.
///
/// JSON-RPC reserved codes (per spec):
///   -32700 parse, -32600 invalid req, -32601 method not found,
///   -32602 invalid params, -32603 internal,
///   -32800 cancelled, -32801 content modified.
fn classify_server_error(code: i32, message: &str, full: String) -> OutputError {
    let lower = message.to_ascii_lowercase();

    // Standard JSON-RPC + LSP reserved codes win first.
    match code {
        -32601 => {
            return OutputError::new(ErrorCode::Unsupported, full).with_hint(
                "Method not implemented by this language server. \
                 Run 'symora doctor' to check supported features.",
            );
        }
        -32602 => {
            return OutputError::new(ErrorCode::InvalidArgument, full);
        }
        -32700 => {
            return OutputError::new(ErrorCode::ParseError, full);
        }
        -32800 => return OutputError::new(ErrorCode::Cancelled, full),
        -32801 => {
            return OutputError::new(ErrorCode::Cancelled, full)
                .with_hint("File content changed mid-request. Retry the same call.");
        }
        _ => {}
    }

    // Fall back to message-pattern matching for the catch-all -32603 / -32000
    // server errors that real LSPs love to throw with prose-only payloads.
    if lower.contains("not found") || lower.contains("no such file") {
        OutputError::new(ErrorCode::NotFound, full)
    } else if lower.contains("not supported")
        || lower.contains("unimplemented")
        || lower.contains("no handler")
    {
        OutputError::new(ErrorCode::Unsupported, full)
    } else if lower.contains("invalid position")
        || lower.contains("invalid offset")
        || lower.contains("out of bounds")
    {
        OutputError::new(ErrorCode::InvalidArgument, full).with_hint(
            "Position is outside file bounds. Verify line/column with 'symora symbols <file>'.",
        )
    } else if lower.contains("content modified") || lower.contains("version mismatch") {
        OutputError::new(ErrorCode::Cancelled, full)
            .with_hint("File changed during the request. Retry the same call.")
    } else if lower.contains("timeout") || lower.contains("timed out") {
        OutputError::new(ErrorCode::Timeout, full)
    } else if lower.contains("cancelled") {
        OutputError::new(ErrorCode::Cancelled, full)
    } else {
        OutputError::new(ErrorCode::Internal, full)
    }
}

impl From<SearchError> for OutputError {
    fn from(err: SearchError) -> Self {
        let message = err.to_string();
        match err {
            SearchError::InvalidPattern(_) => Self::new(ErrorCode::InvalidArgument, message)
                .with_hint("See tree-sitter query syntax: https://tree-sitter.github.io/tree-sitter/using-parsers#pattern-matching-with-queries"),
            SearchError::UnsupportedLanguage(_) => Self::new(ErrorCode::Unsupported, message),
            SearchError::Failed(_) => Self::new(ErrorCode::Internal, message),
        }
    }
}

impl From<StoreError> for OutputError {
    fn from(err: StoreError) -> Self {
        let message = err.to_string();
        match err {
            StoreError::NotInitialized => Self::new(ErrorCode::StoreNotInitialized, message)
                .with_hint("Run: symora search index build"),
            StoreError::AlreadyIndexing | StoreError::Busy | StoreError::Rebuilding => {
                Self::new(ErrorCode::Conflict, message).with_hint("Wait a moment and retry.")
            }
            StoreError::Database(_)
            | StoreError::SchemaMismatch { .. }
            | StoreError::Corrupt(_) => Self::new(ErrorCode::Internal, message),
            StoreError::EmptyScope => Self::new(ErrorCode::InvalidArgument, message).with_hint(
                "Name at least one language, or omit --lang to cover every indexed one.",
            ),
            StoreError::Io(_) => Self::new(ErrorCode::Io, message),
            StoreError::Unreachable(err) => Self::from(*err),
        }
    }
}

impl From<ConfigError> for OutputError {
    fn from(err: ConfigError) -> Self {
        let message = err.to_string();
        match err {
            ConfigError::Parse(_) => Self::new(ErrorCode::ParseError, message),
            ConfigError::NotFound(_) => Self::new(ErrorCode::NotFound, message),
            ConfigError::InvalidValue { .. } => Self::new(ErrorCode::InvalidArgument, message),
            ConfigError::Io(_) => Self::new(ErrorCode::Io, message),
        }
    }
}

impl From<ProjectError> for OutputError {
    fn from(err: ProjectError) -> Self {
        let message = err.to_string();
        match err {
            ProjectError::AlreadyExists(_) => Self::new(ErrorCode::AlreadyExists, message),
            ProjectError::Io(_) => Self::new(ErrorCode::Io, message),
        }
    }
}

impl From<std::io::Error> for OutputError {
    fn from(err: std::io::Error) -> Self {
        Self::new(ErrorCode::Io, err.to_string())
    }
}

/// `?`-propagated errors keep their structured code at every output
/// boundary: each typed error a command can raise is unwrapped back to
/// its dedicated mapping instead of collapsing to `internal`.
impl From<anyhow::Error> for OutputError {
    fn from(err: anyhow::Error) -> Self {
        let err = match err.downcast::<OutputError>() {
            Ok(e) => return e,
            Err(e) => e,
        };
        let err = match err.downcast::<crate::cli::CliInputError>() {
            Ok(e) => return e.into(),
            Err(e) => e,
        };
        let err = match err.downcast::<LspError>() {
            Ok(e) => return e.into(),
            Err(e) => e,
        };
        let err = match err.downcast::<StoreError>() {
            Ok(e) => return e.into(),
            Err(e) => e,
        };
        let err = match err.downcast::<SearchError>() {
            Ok(e) => return e.into(),
            Err(e) => e,
        };
        let err = match err.downcast::<ConfigError>() {
            Ok(e) => return e.into(),
            Err(e) => e,
        };
        let err = match err.downcast::<serde_json::Error>() {
            Ok(e) => return e.into(),
            Err(e) => e,
        };
        let err = match err.downcast::<std::io::Error>() {
            Ok(e) => return e.into(),
            Err(e) => e,
        };
        Self::new(ErrorCode::Internal, err.to_string())
    }
}

impl From<serde_json::Error> for OutputError {
    fn from(err: serde_json::Error) -> Self {
        Self::new(ErrorCode::ParseError, err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server answered well; symora declines to apply the answer's shape.
    /// That is `unsupported` — the agent picks another route — never
    /// `internal`, which reads as a broken tool.
    #[test]
    fn unsupported_edit_maps_to_unsupported() {
        let err: OutputError = LspError::UnsupportedEdit("command-only action".into()).into();
        assert!(matches!(err.code, ErrorCode::Unsupported));
        assert_eq!(err.message, "command-only action");
    }

    #[test]
    fn lsp_error_maps_to_install_hint() {
        let err: OutputError = LspError::ServerNotInstalled {
            name: "rust-analyzer".into(),
            install_hint: "rustup component add rust-analyzer".into(),
        }
        .into();
        assert!(matches!(err.code, ErrorCode::ServerNotInstalled));
        assert!(err.hint.unwrap().contains("rustup"));
    }

    #[test]
    fn store_not_initialized_carries_hint() {
        let err: OutputError = StoreError::NotInitialized.into();
        assert!(matches!(err.code, ErrorCode::StoreNotInitialized));
        assert!(err.hint.unwrap().contains("symora search index build"));
    }

    #[test]
    fn search_invalid_pattern_carries_doc_hint() {
        let err: OutputError = SearchError::InvalidPattern("oops".into()).into();
        assert!(matches!(err.code, ErrorCode::InvalidArgument));
        assert!(err.hint.is_some());
    }

    #[test]
    fn json_serialization_includes_code_and_message() {
        let err = OutputError::not_found("nope").with_hint("try X");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "not_found");
        assert_eq!(json["message"], "nope");
        assert_eq!(json["hint"], "try X");
    }

    /// End-to-end: a `LspError::ServerNotInstalled` born inside the daemon
    /// arrives at the CLI surface as a structured `ServerNotInstalled`
    /// error with the install hint intact. The wire layer carries the
    /// variant via `RpcError.data` (typed `WireLspError`), so the client
    /// reconstructs the typed variant without parsing prose.
    #[test]
    fn daemon_round_trip_preserves_server_not_installed() {
        use crate::daemon::protocol::RpcError;
        use crate::daemon::wire_error::WireLspError;

        // Origin (daemon side): typed variant.
        let origin = LspError::ServerNotInstalled {
            name: "rust-analyzer".into(),
            install_hint: "rustup component add rust-analyzer".into(),
        };

        // Wire: RpcError now carries the structured payload in `data`.
        let rpc: RpcError = (&origin).into();
        let data = rpc.data.expect("structured payload must be attached");
        let wire: WireLspError = serde_json::from_value(data).expect("wire payload deserializes");

        // Client side reconstructs the typed variant verbatim.
        let recovered: LspError = wire.into();

        // OutputError mapping handles ServerNotInstalled directly — no prose
        // parsing involved.
        let out: OutputError = recovered.into();
        assert!(matches!(out.code, ErrorCode::ServerNotInstalled));
        assert_eq!(
            out.hint.as_deref(),
            Some("rustup component add rust-analyzer"),
            "install hint must survive the full round-trip",
        );
    }

    #[test]
    fn server_error_method_not_found_maps_to_unsupported() {
        let err: OutputError = LspError::ServerError {
            code: -32601,
            message: "method textDocument/typeDefinition not found".into(),
        }
        .into();
        assert!(matches!(err.code, ErrorCode::Unsupported));
        assert!(err.hint.is_some());
    }

    #[test]
    fn server_error_invalid_params_maps_to_invalid_argument() {
        let err: OutputError = LspError::ServerError {
            code: -32602,
            message: "invalid params".into(),
        }
        .into();
        assert!(matches!(err.code, ErrorCode::InvalidArgument));
    }

    #[test]
    fn server_error_content_modified_maps_to_cancelled_with_hint() {
        let err: OutputError = LspError::ServerError {
            code: -32801,
            message: "content modified".into(),
        }
        .into();
        assert!(matches!(err.code, ErrorCode::Cancelled));
        assert!(err.hint.unwrap().contains("Retry"));
    }

    #[test]
    fn server_error_invalid_position_message_maps_to_invalid_argument() {
        let err: OutputError = LspError::ServerError {
            code: -32603,
            message: "Invalid position: line 9999 out of bounds".into(),
        }
        .into();
        assert!(matches!(err.code, ErrorCode::InvalidArgument));
        assert!(err.hint.is_some());
    }

    #[test]
    fn server_error_not_found_message_maps_to_not_found() {
        let err: OutputError = LspError::ServerError {
            code: -32603,
            message: "file not found".into(),
        }
        .into();
        assert!(matches!(err.code, ErrorCode::NotFound));
    }

    #[test]
    fn server_error_unsupported_message_maps_to_unsupported() {
        let err: OutputError = LspError::ServerError {
            code: -32603,
            message: "unimplemented method".into(),
        }
        .into();
        assert!(matches!(err.code, ErrorCode::Unsupported));
    }

    #[test]
    fn server_error_unknown_falls_back_to_internal() {
        let err: OutputError = LspError::ServerError {
            code: -32000,
            message: "something cryptic happened".into(),
        }
        .into();
        assert!(matches!(err.code, ErrorCode::Internal));
    }

    #[test]
    fn serialization_skips_none_hint() {
        let err = OutputError::internal("boom");
        let json = serde_json::to_value(&err).unwrap();
        assert!(json.get("hint").is_none());
    }
}
