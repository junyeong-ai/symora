//! Outputs for the analysis commands (`impact`, `context`, `usage`,
//! `diff_impact`). Pure fact data — no heuristic judgements live here.

use std::path::Path;

use serde::Serialize;

use super::{LocationOutput, Section};
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
    /// Whether the target resolved to a real symbol. Emitted only when
    /// `false` — a synthesized `symbol@line:col` placeholder must never be
    /// mistaken for a resolved symbol.
    #[serde(skip_serializing_if = "is_true")]
    pub resolved: bool,
}

fn is_true(b: &bool) -> bool {
    *b
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
            resolved: true,
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
            resolved: true,
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
                Self {
                    resolved: false,
                    ..Self::new(
                        format!("symbol@{}:{}", line, column),
                        "unknown".to_string(),
                        file_str,
                        line,
                    )
                }
            }
        }
    }
}

/// Response for the `refs` command: the resolved target symbol plus its
/// reference list. `target` discloses what the input position snapped to —
/// the same honesty `impact`/`context`/`usage` already provide — so a
/// line-only query is self-describing without a second lookup. The
/// reference `Section` is flattened in, keeping the one list contract.
#[derive(Debug, Serialize)]
pub struct RefsOutput {
    pub target: TargetOutput,
    #[serde(flatten)]
    pub references: Section<LocationOutput>,
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::models::symbol::{Location, SymbolKind};

    #[test]
    fn resolved_is_omitted_for_a_real_symbol() {
        let symbol = Symbol::new(
            "process".to_string(),
            SymbolKind::Function,
            Location::point(PathBuf::from("/proj/src/lib.rs"), 42, 7),
        );
        let target = TargetOutput::from_symbol_or_fallback(
            Some(&symbol),
            Path::new(""),
            0,
            0,
            Path::new("/proj"),
        );
        let value = serde_json::to_value(&target).unwrap();

        assert_eq!(value["name"], "process");
        assert!(value.get("resolved").is_none());
    }

    #[test]
    fn fallback_target_discloses_resolved_false() {
        let target = TargetOutput::from_symbol_or_fallback(
            None,
            Path::new("/proj/src/lib.rs"),
            42,
            7,
            Path::new("/proj"),
        );
        let value = serde_json::to_value(&target).unwrap();

        assert_eq!(value["name"], "symbol@42:7");
        assert_eq!(value["kind"], "unknown");
        assert_eq!(value["resolved"], false);
    }

    #[test]
    fn refs_output_nests_target_and_flattens_the_section() {
        let symbol = Symbol::new(
            "process".to_string(),
            SymbolKind::Function,
            Location::point(PathBuf::from("/proj/src/lib.rs"), 42, 7),
        );
        let target = TargetOutput::from_symbol(&symbol, Path::new("/proj"))
            .with_signature(Some("fn process()".to_string()));
        let out = RefsOutput {
            target,
            references: crate::cli::response::Section::with_total(
                vec![crate::cli::response::LocationOutput::from_path(
                    Path::new("/proj/src/a.rs"),
                    1,
                    2,
                    Path::new("/proj"),
                )],
                1,
            ),
        };
        let value = serde_json::to_value(&out).unwrap();

        // `target` is nested and self-describing.
        assert_eq!(value["target"]["name"], "process");
        assert_eq!(value["target"]["signature"], "fn process()");
        // The reference Section is flattened to the top level (the one list
        // contract), not nested under a `references` key.
        assert_eq!(value["count"], 1);
        assert_eq!(value["showing"], 1);
        assert!(value["items"].is_array());
        assert!(
            value.get("references").is_none(),
            "section must be flattened, not nested under `references`"
        );
    }
}
