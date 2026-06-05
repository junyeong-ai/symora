use serde::{Deserialize, Serialize};

use super::lsp::Range;
use super::symbol::Location;

/// Outcome of one diagnostics pull. `items` is authoritative only when
/// `status` is `Ok`; an empty list under `Unconfirmed` means "unknown",
/// never "clean".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsReport {
    pub status: DiagnosticsStatus,
    pub items: Vec<Diagnostic>,
}

impl DiagnosticsReport {
    pub fn unsupported() -> Self {
        Self {
            status: DiagnosticsStatus::Unsupported,
            items: Vec::new(),
        }
    }
}

/// Whether the diagnostics in a report can be trusted.
///
/// - `Ok` — the server confirmed an analysis of the current content.
/// - `Unconfirmed` — no publish arrived for the synced content within the
///   wait window; the server may still be analyzing.
/// - `Unsupported` — the language's server does not publish diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsStatus {
    Ok,
    Unconfirmed,
    Unsupported,
}

impl DiagnosticsStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticRelatedInfo {
    pub location: Location,
    pub message: String,
}
