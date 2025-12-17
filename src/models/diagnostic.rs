//! Diagnostic model for LSP integration

use serde::{Deserialize, Serialize};

use super::lsp::{Position, Range};
use super::symbol::Location;

/// LSP diagnostic information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file_path: String,
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<DiagnosticTag>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_information: Vec<DiagnosticRelatedInfo>,
}

impl Diagnostic {
    pub fn new(
        file_path: String,
        line: u32,
        column: u32,
        severity: DiagnosticSeverity,
        message: String,
    ) -> Self {
        let pos = Position::new(line, column);
        Self {
            file_path,
            range: Range::point(pos),
            severity,
            message,
            code: None,
            source: None,
            tags: Vec::new(),
            related_information: Vec::new(),
        }
    }

    pub fn display_line(&self) -> u32 {
        self.range.start.line + 1
    }

    pub fn display_column(&self) -> u32 {
        self.range.start.character + 1
    }

    pub fn display_end_line(&self) -> u32 {
        self.range.end.line + 1
    }

    pub fn display_end_column(&self) -> u32 {
        self.range.end.character + 1
    }
}

/// Severity levels (matches LSP spec)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

impl DiagnosticSeverity {
    /// Parse from LSP numeric value
    pub fn from_lsp(value: i64) -> Self {
        match value {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Information,
            _ => Self::Hint,
        }
    }
}

impl std::fmt::Display for DiagnosticSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error => write!(f, "error"),
            Self::Warning => write!(f, "warning"),
            Self::Information => write!(f, "info"),
            Self::Hint => write!(f, "hint"),
        }
    }
}

impl std::str::FromStr for DiagnosticSeverity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "error" | "e" => Ok(Self::Error),
            "warning" | "warn" | "w" => Ok(Self::Warning),
            "info" | "information" | "i" => Ok(Self::Information),
            "hint" | "h" => Ok(Self::Hint),
            _ => Err(format!(
                "Unknown severity: '{}'. Valid: error, warning, info, hint",
                s
            )),
        }
    }
}

/// Diagnostic tag (LSP spec)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticTag {
    Unnecessary = 1,
    Deprecated = 2,
}

impl std::fmt::Display for DiagnosticTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unnecessary => write!(f, "unnecessary"),
            Self::Deprecated => write!(f, "deprecated"),
        }
    }
}

/// Related diagnostic information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticRelatedInfo {
    pub location: Location,
    pub message: String,
}
