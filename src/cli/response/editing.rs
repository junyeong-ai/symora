//! Outputs for editing-side commands (`actions`, `rename`, `edit`).

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct FileChangeOutput {
    pub file: String,
    pub edit_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionOutput {
    pub title: String,
    pub kind: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_preferred: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ApplyActionOutput {
    pub title: String,
    pub kind: String,
    pub applied: bool,
    pub files_changed: usize,
    pub changes: Vec<FileChangeOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
