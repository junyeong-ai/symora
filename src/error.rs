//! Error types for Symora

use thiserror::Error;

use crate::models::symbol::Language;

pub type SymoraResult<T> = std::result::Result<T, SymoraError>;

#[derive(Debug, Error)]
pub enum SymoraError {
    #[error("{0}")]
    Lsp(#[from] LspError),

    #[error("{0}")]
    Search(#[from] SearchError),

    #[error("{0}")]
    Config(#[from] ConfigError),

    #[error("{0}")]
    Project(#[from] ProjectError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum LspError {
    #[error("Failed to start server: {0}")]
    ServerStart(String),

    #[error("Server not connected. Try: symora daemon stop && symora daemon start")]
    NotConnected,

    #[error("Server not installed: {name}. Install: {install_hint}")]
    ServerNotInstalled { name: String, install_hint: String },

    #[error("Unsupported language: {0}. Run 'symora doctor' to see supported languages.")]
    UnsupportedLanguage(String),

    #[error("{language:?} ({server}) does not support '{feature}'. {suggestion}")]
    FeatureNotSupported {
        language: Language,
        server: String,
        feature: String,
        suggestion: String,
    },

    #[error("{language:?} language server terminated unexpectedly")]
    ServerTerminated { language: Language },

    #[error("{0}")]
    Timeout(String),

    #[error("Request cancelled")]
    RequestCancelled,

    #[error("Server error [{code}]: {message}")]
    ServerError { code: i32, message: String },

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("File too large ({size_mb}MB > {limit_mb}MB limit): {path}")]
    FileTooLarge {
        path: String,
        size_mb: u64,
        limit_mb: u64,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl LspError {
    const CANCELLED_ERROR_CODE: i32 = -32800;

    pub fn error_code(&self) -> i32 {
        match self {
            Self::ServerError { code, .. } => *code,
            Self::ServerTerminated { .. } => -32099,
            Self::Timeout(_) => -32001,
            Self::NotConnected => -32003,
            Self::RequestCancelled => Self::CANCELLED_ERROR_CODE,
            _ => -32000,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::RequestCancelled)
            || matches!(self, Self::ServerError { code, .. } if *code == Self::CANCELLED_ERROR_CODE)
    }

    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::ServerTerminated { .. }
                | Self::NotConnected
                | Self::Timeout(_)
                | Self::RequestCancelled
        ) || self.is_cancelled()
    }

    pub fn needs_restart(&self) -> bool {
        matches!(self, Self::ServerTerminated { .. } | Self::NotConnected)
            || self.is_server_shutdown()
    }

    fn is_server_shutdown(&self) -> bool {
        matches!(self, Self::ServerError { message, .. }
            if message.to_lowercase().contains("shutdown")
               || message.to_lowercase().contains("server stopped"))
    }

    /// Get the affected language if this is a server-specific error
    pub fn affected_language(&self) -> Option<Language> {
        match self {
            Self::ServerTerminated { language } => Some(*language),
            Self::FeatureNotSupported { language, .. } => Some(*language),
            _ => None,
        }
    }

    pub fn feature_not_supported(
        language: Language,
        server: &str,
        feature: &str,
        suggestion: &str,
    ) -> Self {
        Self::FeatureNotSupported {
            language,
            server: server.to_string(),
            feature: feature.to_string(),
            suggestion: suggestion.to_string(),
        }
    }

    pub fn server_error_friendly(code: i32, message: String) -> Self {
        let actual_message = message
            .strip_prefix("Server error [")
            .and_then(|s| s.find("]: ").map(|i| s[i + 3..].to_string()))
            .unwrap_or(message);

        let friendly_message = match code {
            -32601 => Self::format_method_not_found(&actual_message),
            -32002 => "Server initializing. Try again in a moment.".to_string(),
            -32603 | -32801 => Self::classify_internal_error(&actual_message),
            _ => actual_message,
        };

        Self::ServerError {
            code,
            message: friendly_message,
        }
    }

    fn format_method_not_found(message: &str) -> String {
        let lower = message.to_lowercase();

        if lower.contains("rename") {
            "Rename not supported. Try: symora search text \"<symbol>\" to find occurrences".into()
        } else if lower.contains("callhierarchy") || lower.contains("call_hierarchy") {
            "Call hierarchy not supported. Try: symora find refs <location>".into()
        } else if lower.contains("implementation") {
            "Find implementations not supported. Try: symora find refs <location>".into()
        } else if lower.contains("typedefinition") || lower.contains("type_definition") {
            "Type definition not supported. Try: symora find def <location>".into()
        } else if lower.contains("preparecallhierarchy") {
            "Call hierarchy not supported. Try: symora find refs <location>".into()
        } else {
            format!("Feature not supported: {}", message)
        }
    }

    fn classify_internal_error(message: &str) -> String {
        let msg = message.trim();
        let lower = msg.to_lowercase();

        if msg.is_empty() || lower == "internal error" || lower == "internal error." {
            return "Operation failed. The position may be invalid.".to_string();
        }

        if lower.starts_with("invalid position")
            || lower.starts_with("file changed")
            || lower.starts_with("file not found")
        {
            return msg.to_string();
        }

        if lower.contains("invalid offset") || lower.contains("out of bounds") {
            "Invalid position: line or column exceeds file bounds.".to_string()
        } else if lower.contains("content modified") || lower.contains("version mismatch") {
            "File changed during operation. Please retry.".to_string()
        } else if lower.contains("not found") || lower.contains("no such file") {
            "File not found or inaccessible.".to_string()
        } else if lower.contains("timeout") {
            "Request timed out. The language server may be busy.".to_string()
        } else if lower.contains("not supported") || lower.contains("unimplemented") {
            format!("Feature not available: {}", msg)
        } else {
            msg.to_string()
        }
    }
}

impl From<crate::infra::lsp::protocol::ResponseError> for LspError {
    fn from(err: crate::infra::lsp::protocol::ResponseError) -> Self {
        LspError::server_error_friendly(err.code, err.message)
    }
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("Invalid AST pattern: {0}. See tree-sitter query syntax.")]
    InvalidPattern(String),

    #[error("AST search not supported for {0:?}")]
    UnsupportedLanguage(Language),

    #[error("Search failed: {0}")]
    Failed(String),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Config parse error: {0}")]
    Parse(String),

    #[error("{0}")]
    NotFound(String),

    #[error("Invalid value for '{key}': {message}")]
    InvalidValue { key: String, message: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("Project already initialized at: {0}")]
    AlreadyExists(std::path::PathBuf),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_terminated_error() {
        let err = LspError::ServerTerminated {
            language: Language::Rust,
        };
        assert!(err.is_recoverable());
        assert_eq!(err.affected_language(), Some(Language::Rust));
        assert_eq!(err.error_code(), -32099);
    }

    #[test]
    fn test_not_connected_is_recoverable() {
        let err = LspError::NotConnected;
        assert!(err.is_recoverable());
    }

    #[test]
    fn test_timeout_is_recoverable() {
        let err = LspError::Timeout("test".to_string());
        assert!(err.is_recoverable());
        assert!(!err.needs_restart());
    }

    #[test]
    fn test_server_terminated_needs_restart() {
        let err = LspError::ServerTerminated {
            language: Language::Rust,
        };
        assert!(err.is_recoverable());
        assert!(err.needs_restart());
    }

    #[test]
    fn test_feature_not_supported_has_language() {
        let err = LspError::FeatureNotSupported {
            language: Language::Python,
            server: "pyright".to_string(),
            feature: "callHierarchy".to_string(),
            suggestion: "Use find references".to_string(),
        };
        assert!(!err.is_recoverable());
        assert_eq!(err.affected_language(), Some(Language::Python));
    }

    #[test]
    fn test_cancelled_error() {
        let err = LspError::RequestCancelled;
        assert!(err.is_cancelled());
        assert!(err.is_recoverable());

        let server_cancelled = LspError::ServerError {
            code: -32800,
            message: "cancelled".to_string(),
        };
        assert!(server_cancelled.is_cancelled());
        assert!(server_cancelled.is_recoverable());
    }

    #[test]
    fn test_server_shutdown_needs_restart() {
        let shutdown_err = LspError::ServerError {
            code: -32800,
            message: "Server shutdown".to_string(),
        };
        assert!(shutdown_err.needs_restart());
        assert!(shutdown_err.is_recoverable());

        let stopped_err = LspError::ServerError {
            code: -32000,
            message: "Server stopped unexpectedly".to_string(),
        };
        assert!(stopped_err.needs_restart());
    }

    #[test]
    fn test_classify_internal_error() {
        assert_eq!(
            LspError::classify_internal_error("internal error"),
            "Operation failed. The position may be invalid."
        );
        assert_eq!(
            LspError::classify_internal_error("invalid offset 100"),
            "Invalid position: line or column exceeds file bounds."
        );
        assert_eq!(
            LspError::classify_internal_error("content modified"),
            "File changed during operation. Please retry."
        );
    }
}
