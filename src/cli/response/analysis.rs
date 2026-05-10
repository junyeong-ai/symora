//! Outputs for the analysis commands (`impact`, `context`, `usage`,
//! `diff_impact`). Pure fact data — no heuristic judgements live here.

use std::path::Path;

use serde::Serialize;

use super::LocationOutput;
use crate::models::symbol::Symbol;

/// Target symbol info (unified across `impact`, `context`, etc.).
#[derive(Debug, Serialize)]
pub struct TargetOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl TargetOutput {
    pub fn new(name: String, kind: String, file: String, line: u32) -> Self {
        Self {
            name,
            kind,
            file,
            line,
            signature: None,
            body: None,
        }
    }

    pub fn from_symbol(symbol: &Symbol, root: &Path) -> Self {
        let file = symbol
            .location
            .file
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| symbol.location.file.display().to_string());

        Self {
            name: symbol.name.clone(),
            kind: symbol.kind.to_string(),
            file,
            line: symbol.location.line,
            signature: None,
            body: None,
        }
    }

    pub fn with_signature(mut self, signature: Option<String>) -> Self {
        self.signature = signature;
        self
    }

    pub fn with_body(mut self, body: Option<String>) -> Self {
        self.body = body;
        self
    }

    pub fn from_symbol_or_fallback(
        symbol: Option<&Symbol>,
        file: &Path,
        line: u32,
        column: u32,
        root: &Path,
    ) -> Self {
        match symbol {
            Some(sym) => Self::from_symbol(sym, root),
            None => {
                let file_str = file
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| file.display().to_string());
                Self::new(
                    format!("symbol@{}:{}", line, column),
                    "unknown".to_string(),
                    file_str,
                    line,
                )
            }
        }
    }
}

/// Reference statistics (counts, no judgements).
#[derive(Debug, Serialize)]
pub struct RefOutput {
    pub total: usize,
    pub test: usize,
    pub prod: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modules: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_exported: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct TestOutput {
    pub name: String,
    pub location: LocationOutput,
}

#[derive(Debug, Serialize)]
pub struct TestCoverageOutput {
    pub count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AffectedFileOutput {
    pub file: String,
    pub is_test: bool,
    pub refs: usize,
}

/// Response for the `impact` command.
#[derive(Debug, Serialize)]
pub struct ImpactOutput {
    pub target: TargetOutput,
    pub refs: RefOutput,
    pub coverage: TestCoverageOutput,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<AffectedFileOutput>,
    /// Transitive caller graph + risk + confidence. Absent only when the
    /// LSP failed to start a call hierarchy at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blast_radius: Option<crate::cli::blast_radius::BlastRadius>,
}
