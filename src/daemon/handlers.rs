//! RPC Handler Infrastructure
//!
//! Parameter and response types for daemon handlers.

use serde::{Deserialize, Serialize};

// ============================================================================
// Request Parameter Types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct PositionParams {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub struct FileParams {
    pub file: String,
    pub project: String,
    #[serde(default)]
    pub body: bool,
    #[serde(default)]
    pub depth: u32,
}

#[derive(Debug, Deserialize)]
pub struct RenameParams {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub new_name: String,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceSymbolParams {
    pub query: String,
    pub project: String,
    pub language: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyActionParams {
    pub file: String,
    pub project: String,
    pub action: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct RangeParams {
    pub file: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub struct SelectionRangeParams {
    pub file: String,
    pub positions: Vec<PositionInput>,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub struct PositionInput {
    pub line: u32,
    pub column: u32,
}

// ============================================================================
// Response Types (handler-specific JSON shapes)
// ============================================================================

#[derive(Serialize)]
pub struct CodeActionJson {
    pub title: String,
    pub kind: String,
    pub is_preferred: bool,
    pub diagnostics: Vec<String>,
}

#[derive(Serialize)]
pub struct FileChangeJson {
    pub file: String,
    pub edit_count: usize,
}

#[derive(Serialize)]
pub struct TypeHierarchyItemJson {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize)]
pub struct InlayHintJson {
    pub line: u32,
    pub character: u32,
    pub label: String,
    pub kind: u32,
    pub padding_left: bool,
    pub padding_right: bool,
}

#[derive(Serialize)]
pub struct FoldingRangeJson {
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_character: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_character: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapsed_text: Option<String>,
}

#[derive(Serialize)]
pub struct SelectionRangeJson {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Box<SelectionRangeJson>>,
}

#[derive(Serialize)]
pub struct CodeLensJson {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CodeLensCommandJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct CodeLensCommandJson {
    pub title: String,
    pub command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<serde_json::Value>,
}
