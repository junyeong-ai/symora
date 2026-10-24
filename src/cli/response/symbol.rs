use std::path::Path;

use serde::Serialize;

use super::LocationOutput;
use crate::models::symbol::Symbol;

/// Symbol output for find symbol commands.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolOutput {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    pub kind: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_location: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// With `--refs`: references that name the declaration, all of them and
    /// split by test and production code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_refs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prod_refs: Option<usize>,
    /// Present when the counts are not authoritative: `unavailable` (the
    /// reference query failed, so they are absent rather than zero) or
    /// `indexing_degraded` (they are a lower bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_omitted: Option<super::BodyOmission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<SymbolOutput>>,
}

/// The source a symbol row carries beside its location. A body is the whole
/// definition and is emitted with its signature, so asking for both asks for
/// the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolSource {
    Location,
    Signature,
    Body,
}

impl SymbolSource {
    pub fn new(body: bool, signature: bool) -> Self {
        match (body, signature) {
            (true, _) => Self::Body,
            (false, true) => Self::Signature,
            (false, false) => Self::Location,
        }
    }

    /// The option that asks a command for this source, with its leading space.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Location => "",
            Self::Signature => " --signature",
            Self::Body => " --body",
        }
    }
}

impl SymbolOutput {
    pub fn body_paths(items: &[Self]) -> Vec<String> {
        fn collect(items: &[SymbolOutput], prefix: &str, paths: &mut Vec<String>) {
            for (index, item) in items.iter().enumerate() {
                let path = format!("{prefix}/{index}");
                if item.body.is_some() {
                    paths.push(format!("{path}/body"));
                }
                if let Some(children) = &item.children {
                    collect(children, &format!("{path}/children"), paths);
                }
            }
        }
        let mut paths = Vec::new();
        collect(items, "/items", &mut paths);
        paths
    }

    pub fn from_symbol(symbol: &Symbol, root: &Path) -> Self {
        Self::project(symbol, root, false)
    }

    pub fn from_symbol_signature(symbol: &Symbol, root: &Path) -> Self {
        Self::project(symbol, root, true)
    }

    fn project(symbol: &Symbol, root: &Path, signature_only: bool) -> Self {
        Self {
            name: symbol.name.clone(),
            name_path: symbol.name_path.clone(),
            kind: symbol.kind.to_string(),
            // from_location carries degraded_column: a workspace/symbol result
            // is cross-file and may be decoded against an unreadable line. The
            // end_location is a synthesized end position (no separate flag).
            location: LocationOutput::from_location(&symbol.location, root),
            end_location: symbol.location.end_line.map(|end_line| {
                LocationOutput::from_path(
                    &symbol.location.file,
                    end_line,
                    symbol.location.end_column.unwrap_or(1),
                    root,
                )
            }),
            container: symbol.container.clone(),
            refs: None,
            test_refs: None,
            prod_refs: None,
            refs_status: None,
            signature: if signature_only {
                symbol.signature.clone()
            } else {
                None
            },
            documentation: None,
            body: if signature_only {
                None
            } else {
                symbol.body.clone()
            },
            body_omitted: None,
            children: if symbol.children.is_empty() {
                None
            } else {
                Some(
                    symbol
                        .children
                        .iter()
                        .map(|s| Self::project(s, root, signature_only))
                        .collect(),
                )
            },
        }
    }

    pub fn with_signature(mut self, signature: Option<String>) -> Self {
        self.signature = signature;
        self
    }

    pub fn without_body(mut self) -> Self {
        self.body = None;
        self.children = self
            .children
            .map(|children| children.into_iter().map(Self::without_body).collect());
        self
    }

    pub fn without_children(mut self) -> Self {
        self.children = None;
        self
    }
}

/// Status of a configured language server (used by `doctor`).
#[derive(Debug, Serialize)]
pub struct ServerStatusOutput {
    pub language: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
}
