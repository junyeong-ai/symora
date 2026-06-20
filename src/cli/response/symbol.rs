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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<SymbolOutput>>,
}

impl SymbolOutput {
    pub fn from_symbol(symbol: &Symbol, root: &Path) -> Self {
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
            signature: None,
            documentation: None,
            body: symbol.body.clone(),
            children: if symbol.children.is_empty() {
                None
            } else {
                Some(
                    symbol
                        .children
                        .iter()
                        .map(|s| SymbolOutput::from_symbol(s, root))
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
