use serde::Deserialize;

fn default_depth() -> u32 {
    u32::MAX
}

#[derive(Debug, Deserialize)]
pub(crate) struct PositionParams {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProjectParams {
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FileParams {
    pub file: String,
    pub project: String,
    #[serde(default)]
    pub body: bool,
    #[serde(default = "default_depth")]
    pub depth: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RenameParams {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub new_name: String,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceSymbolParams {
    pub query: String,
    pub project: String,
    pub language: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApplyActionParams {
    pub file: String,
    pub project: String,
    pub action: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InlayHintsParams {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SelectionRangeParams {
    pub file: String,
    pub positions: Vec<PositionInput>,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PositionInput {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchSymbolsParams {
    pub project: String,
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchContentParams {
    pub project: String,
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub language: Option<String>,
}

/// A batch of files symora just wrote — shared by `refresh_files` (store
/// re-index) and `note_files_edited` (LSP-layer note), so multi-file
/// operations (rename, actions apply) cost one request, not one per file.
#[derive(Debug, Deserialize)]
pub(crate) struct EditedFilesParams {
    pub files: Vec<String>,
    pub project: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LanguageStatusParams {
    pub project: String,
    pub language: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IndexBuildParams {
    pub project: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub languages: Option<Vec<String>>,
}
