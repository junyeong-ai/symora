//! Outputs derived from raw LSP responses (definition, hover, diagnostics,
//! call/type hierarchies, signature help).

use std::path::Path;

use serde::Serialize;

use super::LocationOutput;

#[derive(Debug, Serialize)]
pub struct DefinitionOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HoverOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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

#[derive(Debug, Serialize)]
pub struct CallHierarchyOutput {
    pub name: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<LocationOutput>,
}

impl CallHierarchyOutput {
    pub fn from_item(item: &crate::models::lsp::CallHierarchyItem, root: &Path) -> Self {
        Self {
            name: item.name.clone(),
            location: LocationOutput::from_path(
                &item.location.file,
                item.location.line,
                item.location.column,
                root,
            ),
            call_site: item
                .call_site
                .as_ref()
                .map(|cs| LocationOutput::from_path(&cs.file, cs.line, cs.column, root)),
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
}

impl TypeInfoOutput {
    pub fn from_item(item: &crate::models::lsp::TypeHierarchyItem, root: &Path) -> Self {
        Self {
            name: item.name.clone(),
            kind: item.kind.to_string(),
            location: LocationOutput::from_path(
                &item.location.file,
                item.location.line,
                item.location.column,
                root,
            ),
            detail: item.detail.clone(),
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
