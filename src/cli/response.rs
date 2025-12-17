//! Response types for CLI output
//!
//! Defines shared response types for commands.
//! All types implement Serialize for consistent JSON output.

use std::path::Path;

use serde::Serialize;

use crate::models::symbol::Symbol;

/// Location in a file (relative path by default)
#[derive(Debug, Clone, Serialize)]
pub struct LocationOutput {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

impl LocationOutput {
    pub fn new(file: impl Into<String>, line: u32, column: u32) -> Self {
        Self {
            file: file.into(),
            line,
            column,
        }
    }

    /// Create from absolute path, converting to relative if within root
    pub fn from_path(path: &Path, line: u32, column: u32, root: &Path) -> Self {
        let file = path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());

        Self { file, line, column }
    }
}

/// Symbol output for find symbol command
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
            location: LocationOutput::from_path(
                &symbol.location.file,
                symbol.location.line,
                symbol.location.column,
                root,
            ),
            end_location: symbol.location.end_line.map(|end_line| {
                LocationOutput::from_path(
                    &symbol.location.file,
                    end_line,
                    symbol.location.end_column.unwrap_or(1),
                    root,
                )
            }),
            container: symbol.container.clone(),
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
}

/// Response for find symbol command
#[derive(Debug, Serialize)]
pub struct SymbolsResponse {
    pub count: usize,
    pub symbols: Vec<SymbolOutput>,
}

/// Response for find refs command
#[derive(Debug, Serialize)]
pub struct ReferencesResponse {
    pub count: usize,
    pub references: Vec<LocationOutput>,
}

/// Response for find def command
#[derive(Debug, Serialize)]
pub struct DefinitionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Response for hover command
#[derive(Debug, Serialize)]
pub struct HoverResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Diagnostic output
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

/// Response for diagnostics command
#[derive(Debug, Serialize)]
pub struct DiagnosticsResponse {
    pub file: String,
    pub count: usize,
    pub diagnostics: Vec<DiagnosticOutput>,
}

/// Call hierarchy item output
#[derive(Debug, Serialize)]
pub struct CallHierarchyOutput {
    pub name: String,
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<LocationOutput>,
}

/// Response for calls command
#[derive(Debug, Serialize)]
pub struct CallsResponse {
    pub direction: String,
    pub count: usize,
    pub calls: Vec<CallHierarchyOutput>,
}

/// Impact file output
#[derive(Debug, Serialize)]
pub struct ImpactFileOutput {
    pub file: String,
    pub reference_count: usize,
    pub references: Vec<ImpactReferenceOutput>,
}

#[derive(Debug, Serialize)]
pub struct ImpactReferenceOutput {
    pub line: u32,
    pub column: u32,
}

/// Response for impact command
#[derive(Debug, Serialize)]
pub struct ImpactResponse {
    pub target: LocationOutput,
    pub depth: u32,
    pub total_references: usize,
    pub affected_files_count: usize,
    pub affected_files: Vec<ImpactFileOutput>,
}

/// Project status output
#[derive(Debug, Serialize)]
pub struct ProjectStatusOutput {
    pub initialized: bool,
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub languages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub servers: Option<Vec<ServerStatusOutput>>,
}

#[derive(Debug, Serialize)]
pub struct ServerStatusOutput {
    pub language: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
}

/// Response for status command
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub project: ProjectStatusOutput,
}

/// Config output
#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Parameter information for signature help
#[derive(Debug, Serialize)]
pub struct ParameterOutput {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

/// Signature information
#[derive(Debug, Serialize)]
pub struct SignatureOutput {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    pub parameters: Vec<ParameterOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

/// Response for signature help command
#[derive(Debug, Serialize)]
pub struct SignatureHelpResponse {
    pub signatures: Vec<SignatureOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}
