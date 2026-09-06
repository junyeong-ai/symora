//! Outputs derived from raw LSP responses (definition, hover, diagnostics,
//! call/type hierarchies, signature help).

use std::path::Path;

use serde::Serialize;

use super::LocationOutput;
use crate::models::lsp::IndexingDegradation;

/// An answer whose completeness depends on workspace indexing. A scalar
/// "nothing here" and a settled "nothing here" are the same JSON without
/// this, so the helper that produces one attaches the state it ran under.
pub trait DisclosesIndexing {
    fn with_indexing(self, indexing: Option<IndexingDegradation>) -> Self;
}

#[derive(Debug, Serialize)]
pub struct DefinitionOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<IndexingDegradation>,
}

#[derive(Debug, Serialize)]
pub struct HoverOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<IndexingDegradation>,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticOutput {
    pub severity: String,
    pub message: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl From<&crate::models::diagnostic::Diagnostic> for DiagnosticOutput {
    fn from(d: &crate::models::diagnostic::Diagnostic) -> Self {
        Self {
            severity: d.severity.to_string(),
            message: d.message.clone(),
            line: d.display_line(),
            column: d.display_column(),
            end_line: d.display_end_line(),
            end_column: d.display_end_column(),
            code: d.code.clone(),
            source: d.source.clone(),
            tags: d.tags.iter().map(|t| t.to_string()).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CallHierarchyOutput {
    pub name: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<LocationOutput>,
    /// Complete verbatim source body, set only by `context --with-bodies`
    /// callee attachment — absent everywhere else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl CallHierarchyOutput {
    pub fn from_item(item: &crate::models::lsp::CallHierarchyItem, root: &Path) -> Self {
        Self {
            name: item.name.clone(),
            location: LocationOutput::from_location(&item.location, root),
            call_site: item
                .call_site
                .as_ref()
                .map(|cs| LocationOutput::from_location(cs, root)),
            body: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeInfoOutput {
    pub name: String,
    pub kind: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Complete verbatim source body, set only by `context --with-bodies`
    /// type attachment — absent everywhere else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl TypeInfoOutput {
    pub fn from_item(item: &crate::models::lsp::TypeHierarchyItem, root: &Path) -> Self {
        Self {
            name: item.name.clone(),
            kind: item.kind.to_string(),
            location: LocationOutput::from_location(&item.location, root),
            detail: item.detail.clone(),
            body: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SignatureHelpOutput {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<SignatureItemOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<IndexingDegradation>,
}

#[derive(Debug, Serialize)]
pub struct SignatureItemOutput {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<ParameterOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct ParameterOutput {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

macro_rules! discloses_indexing {
    ($($type:ty),+ $(,)?) => {
        $(impl DisclosesIndexing for $type {
            fn with_indexing(mut self, indexing: Option<IndexingDegradation>) -> Self {
                self.indexing = indexing;
                self
            }
        })+
    };
}

discloses_indexing!(DefinitionOutput, HoverOutput, SignatureHelpOutput);
