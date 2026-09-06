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
    /// The unresolved-anchor disclosure, omitted when the target resolved: a
    /// synthesized `symbol@line:col` placeholder carries `"binding"` (a
    /// declaration the symbol tree does not list — the placeholder sits at
    /// it), `"not_a_symbol"` (the position was checked and denotes nothing),
    /// or `"unavailable"` (a read failed). One shared `*_status` vocabulary
    /// across every surface (see `AnchorResolution::as_status`), so a
    /// placeholder is never mistaken for a resolved symbol and a read failure
    /// is never reported as "not a symbol".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_status: Option<&'static str>,
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
            anchor_status: None,
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
            anchor_status: None,
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

    /// When `symbol` is `None`, `anchor_status` carries WHY it did not resolve
    /// (`AnchorResolution::as_status`: "binding", "not_a_symbol", or
    /// "unavailable") and is recorded on the placeholder. A resolved symbol
    /// ignores it (status omitted).
    pub fn from_symbol_or_fallback(
        symbol: Option<&Symbol>,
        file: &Path,
        line: u32,
        column: u32,
        root: &Path,
        anchor_status: Option<&'static str>,
    ) -> Self {
        match symbol {
            Some(sym) => Self::from_symbol(sym, root),
            None => {
                let file_str = file
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| file.display().to_string());
                Self {
                    anchor_status,
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
/// reference list. `target` discloses what the input position resolved to —
/// the same honesty `impact`/`context`/`usage` already provide — so a
/// line-only query is self-describing without a second lookup. The
/// reference `Section` is flattened in, keeping the one list contract.
#[derive(Debug, Serialize)]
pub struct RefsOutput {
    pub target: TargetOutput,
    /// Carries the reference list's own `incomplete` — set when the language
    /// server's reference set omits the very usage the query was made from,
    /// so the count is a lower bound. The same disclosure `RefOutput` carries
    /// for `impact`/`context`.
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
    /// Present only when the reference query ran under degraded workspace
    /// indexing — the counts above are then a lower bound, not authoritative.
    /// The same disclosure the `refs` command carries on its references
    /// `Section`, kept here because `impact`/`context` summarize that query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<crate::models::lsp::IndexingDegradation>,
    /// Present (true) only when the language server's reference set omits
    /// the very usage the query was made from — the counts above are then a
    /// lower bound. Some servers leave out the usages of certain bindings
    /// (rust-analyzer does for the parameters of async functions); this is
    /// how the omission is disclosed on every surface that publishes the set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
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
    /// What the counts above do not settle. Omitted when they settle it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    /// Ready-to-run follow-ups, emitted only when a disclosure above says
    /// the analysis is incomplete (depth cap, unfolded dynamic dispatch,
    /// truncated file list) or concentrated in one file — never boilerplate.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next_commands: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::models::symbol::{Location, SymbolKind};

    #[test]
    fn anchor_status_is_omitted_for_a_real_symbol() {
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
            None,
        );
        let value = serde_json::to_value(&target).unwrap();

        assert_eq!(value["name"], "process");
        // A resolved symbol omits the anchor_status disclosure entirely.
        assert!(value.get("anchor_status").is_none());
    }

    /// A position checked and found NOT to be a symbol: `anchor_status` is
    /// "not_a_symbol" on the placeholder — the empty answer is authoritative.
    #[test]
    fn fallback_target_discloses_not_a_symbol() {
        let target = TargetOutput::from_symbol_or_fallback(
            None,
            Path::new("/proj/src/lib.rs"),
            42,
            7,
            Path::new("/proj"),
            Some("not_a_symbol"),
        );
        let value = serde_json::to_value(&target).unwrap();

        assert_eq!(value["name"], "symbol@42:7");
        assert_eq!(value["kind"], "unknown");
        assert_eq!(value["anchor_status"], "not_a_symbol");
    }

    /// A symbol read that was unavailable: `anchor_status:"unavailable"`,
    /// distinct from "not_a_symbol" — the empty answer is "unknown", not an
    /// authoritative "not a symbol".
    #[test]
    fn fallback_target_discloses_unavailable_distinctly() {
        let target = TargetOutput::from_symbol_or_fallback(
            None,
            Path::new("/proj/src/lib.rs"),
            42,
            7,
            Path::new("/proj"),
            Some("unavailable"),
        );
        let value = serde_json::to_value(&target).unwrap();

        assert_eq!(value["anchor_status"], "unavailable");
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
