//! JSON output contract for every CLI command.
//!
//! Each submodule groups outputs by the layer that produces them:
//!   - `symbol`: file/workspace symbol listings.
//!   - `lsp`: LSP-derived responses (definition, hover, hierarchies, etc.).
//!   - `analysis`: derived analytics (impact, refs, tests, coverage).
//!   - `editing`: code-action and rename results.
//!
//! Everything is re-exported flat so existing call sites use
//! `crate::cli::response::SymbolOutput` regardless of which submodule
//! defines it.

mod analysis;
mod editing;
mod lsp;
mod symbol;

pub use analysis::{
    AffectedFileOutput, ImpactOutput, RefOutput, TargetOutput, TestCoverageOutput, TestOutput,
};
pub use editing::{ActionOutput, ApplyActionOutput, FileChangeOutput};
pub use lsp::{
    CallHierarchyOutput, DefinitionOutput, DiagnosticOutput, HoverOutput, ParameterOutput,
    SignatureHelpOutput, SignatureItemOutput, TypeInfoOutput,
};
pub use symbol::{ServerStatusOutput, SymbolOutput};

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::errors::OutputError;

/// List-shaped response wrapper — the one list contract every command
/// (and the daemon wire) emits:
///
/// - `count` — total matches found
/// - `showing` — number actually emitted in `items`
/// - `items` — the result array
/// - `truncated` — present (and `true`) only when `showing < count`
/// - `hints` / `next_commands` — omitted when empty
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section<T> {
    pub count: usize,
    pub showing: usize,
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_commands: Vec<String>,
    /// Present only when the answer was computed under degraded
    /// workspace indexing — the list is then a lower bound, not a
    /// complete enumeration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<crate::models::lsp::IndexingDegradation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OutputError>,
}

impl<T> Section<T> {
    /// Complete result set — nothing was withheld.
    pub fn new(items: Vec<T>) -> Self {
        Self::with_total_count(items, None)
    }

    /// `items` after an emission cap; `count` is the total the command
    /// found before capping. `truncated` derives from `showing < count`.
    pub fn with_total(items: Vec<T>, count: usize) -> Self {
        Self::with_total_count(items, Some(count))
    }

    pub fn error(error: impl Into<OutputError>) -> Self {
        Self {
            count: 0,
            showing: 0,
            items: vec![],
            truncated: false,
            hints: vec![],
            next_commands: vec![],
            indexing: None,
            error: Some(error.into()),
        }
    }

    pub fn with_hints(mut self, hints: Vec<String>) -> Self {
        self.hints = hints;
        self
    }

    pub fn with_next_commands(mut self, next_commands: Vec<String>) -> Self {
        self.next_commands = next_commands;
        self
    }

    pub fn with_indexing(
        mut self,
        indexing: Option<crate::models::lsp::IndexingDegradation>,
    ) -> Self {
        self.indexing = indexing;
        self
    }

    fn with_total_count(items: Vec<T>, count: Option<usize>) -> Self {
        let showing = items.len();
        let count = count.map_or(showing, |c| c.max(showing));
        Self {
            count,
            showing,
            items,
            truncated: showing < count,
            hints: vec![],
            next_commands: vec![],
            indexing: None,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_section_emits_count_showing_and_no_truncation() {
        let value = serde_json::to_value(Section::new(vec![1, 2, 3])).unwrap();
        assert_eq!(value["count"], 3);
        assert_eq!(value["showing"], 3);
        assert_eq!(value["items"], serde_json::json!([1, 2, 3]));
        assert!(value.get("truncated").is_none());
        assert!(value.get("hints").is_none());
        assert!(value.get("next_commands").is_none());
        assert!(value.get("error").is_none());
    }

    #[test]
    fn capped_section_derives_truncated_from_showing_lt_count() {
        let value = serde_json::to_value(Section::with_total(vec![1, 2], 10)).unwrap();
        assert_eq!(value["count"], 10);
        assert_eq!(value["showing"], 2);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn count_never_underreports_emitted_items() {
        let section = Section::with_total(vec![1, 2, 3], 1);
        assert_eq!(section.count, 3);
        assert!(!section.truncated);
    }

    #[test]
    fn hints_and_next_commands_serialize_only_when_present() {
        let value = serde_json::to_value(
            Section::new(vec![1])
                .with_hints(vec!["narrow it".to_string()])
                .with_next_commands(vec!["symora map file src/a.rs".to_string()]),
        )
        .unwrap();
        assert_eq!(value["hints"][0], "narrow it");
        assert_eq!(value["next_commands"][0], "symora map file src/a.rs");
    }

    #[test]
    fn indexing_marker_serializes_only_when_degraded() {
        let degraded = serde_json::to_value(
            Section::new(vec![1])
                .with_indexing(Some(crate::models::lsp::IndexingDegradation::TimedOut)),
        )
        .unwrap();
        assert_eq!(degraded["indexing"], "timed_out");

        let healthy = serde_json::to_value(Section::new(vec![1]).with_indexing(None)).unwrap();
        assert!(healthy.get("indexing").is_none());
    }

    #[test]
    fn section_round_trips_through_the_wire() {
        let wire = serde_json::to_value(Section::with_total(vec![1, 2], 7)).unwrap();
        let parsed: Section<i32> = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed.count, 7);
        assert_eq!(parsed.showing, 2);
        assert!(parsed.truncated);
        assert!(parsed.hints.is_empty());
    }

    #[test]
    fn error_section_is_empty_and_structured() {
        let value = serde_json::to_value(Section::<i32>::error(
            crate::cli::OutputError::not_found("nope"),
        ))
        .unwrap();
        assert_eq!(value["count"], 0);
        assert_eq!(value["showing"], 0);
        assert_eq!(value["error"]["code"], "not_found");
    }
}

/// File location with optional source snippet (relative path by default).
#[derive(Debug, Clone, Serialize)]
pub struct LocationOutput {
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

impl LocationOutput {
    /// Create from absolute path, converting to relative when within `root`.
    pub fn from_path(path: &Path, line: u32, column: u32, root: &Path) -> Self {
        let file = path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());

        Self {
            file,
            line,
            column,
            snippet: None,
        }
    }
}
