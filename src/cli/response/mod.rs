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

use serde::Serialize;

use super::errors::OutputError;

/// List-shaped response wrapper. Every command that returns multiple
/// items wraps them in a `Section` so callers can pattern-match on
/// `count` / `truncated` / `error` without inspecting per-command shapes.
#[derive(Debug, Clone, Serialize)]
pub struct Section<T> {
    pub count: usize,
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OutputError>,
}

impl<T> Section<T> {
    pub fn new(items: Vec<T>) -> Self {
        let count = items.len();
        Self {
            count,
            items,
            truncated: None,
            error: None,
        }
    }

    pub fn with_limit(items: Vec<T>, total: usize) -> Self {
        let count = items.len();
        let truncated = if count < total { Some(true) } else { None };
        Self {
            count,
            items,
            truncated,
            error: None,
        }
    }

    pub fn error(error: impl Into<OutputError>) -> Self {
        Self {
            count: 0,
            items: vec![],
            truncated: None,
            error: Some(error.into()),
        }
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
