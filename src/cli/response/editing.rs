//! Outputs for editing-side commands (`actions`, `rename`, `edit`).

use serde::Serialize;

use super::lsp::DiagnosticOutput;
use super::{LocationOutput, Section};

/// The one output shape every `edit` subcommand emits. One splice, one
/// record: symbol-targeted operations carry `target_*`; raw range and
/// pattern operations omit them.
#[derive(Debug, Serialize)]
pub struct EditOutput {
    /// Which operation ran: `replace_body`, `insert_before`,
    /// `insert_after`, `delete`, `replace`, or `pattern`.
    pub operation: &'static str,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_kind: Option<String>,
    /// Affected line span in the original file: the replaced span for
    /// replacements, the anchor span for inserts.
    pub lines: LineRange,
    pub bytes_changed: i64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// Exact diff hunk, present on dry runs. Derived from the splice
    /// itself, so it never misreports unchanged trailing lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// `delete` only: references outside the deleted span that would
    /// dangle. Present (count 0 included) whenever the check ran —
    /// absent means it could not run; `references_status` says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dangling_references: Option<Section<LocationOutput>>,
    /// `delete` only, and only when the reference check could not run:
    /// `unsupported` (language server lacks references) or
    /// `unavailable` (the lookup failed). Never paired with a list —
    /// an empty list is a real "none found", not a fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references_status: Option<&'static str>,
    /// Post-edit diagnostics, present only when `--with-diagnostics`
    /// was passed on an applied (non-dry-run) edit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<EditDiagnostics>,
}

/// Post-edit diagnostics pull. `status` is always present so an empty
/// list is never mistaken for a verified-clean file:
/// `ok` (server confirmed analyzing the written content),
/// `unconfirmed` (no confirmation within the wait window),
/// `unsupported` (language server doesn't publish diagnostics),
/// `unavailable` (the pull itself failed).
#[derive(Debug, Serialize)]
pub struct EditDiagnostics {
    pub status: &'static str,
    pub count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<DiagnosticOutput>,
}

#[derive(Debug, Serialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

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
