//! Response types for CLI output

use std::path::Path;

use serde::Serialize;

use crate::models::symbol::Symbol;

/// Generic section for list responses
#[derive(Debug, Clone, Serialize)]
pub struct Section<T> {
    pub count: usize,
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            count: 0,
            items: vec![],
            truncated: None,
            error: Some(msg.into()),
        }
    }
}

/// Location in a file (relative path by default)
#[derive(Debug, Clone, Serialize)]
pub struct LocationOutput {
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

impl LocationOutput {
    pub fn new(file: impl Into<String>, line: u32, column: u32) -> Self {
        Self {
            file: file.into(),
            line,
            column,
            snippet: None,
        }
    }

    /// Create from absolute path, converting to relative if within root
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

    pub fn with_snippet(mut self, snippet: String) -> Self {
        self.snippet = Some(snippet);
        self
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

impl CallHierarchyOutput {
    /// Convert from internal CallHierarchyItem to output format
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

/// Target symbol info (unified for impact/context commands)
#[derive(Debug, Serialize)]
pub struct TargetInfo {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl TargetInfo {
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
}

/// Reference summary (minimal pure fact data)
#[derive(Debug, Serialize)]
pub struct RefsSummary {
    /// Total references
    pub total: usize,
    /// Test code references
    pub test: usize,
    /// Production code references
    pub prod: usize,
}

/// Extended reference statistics (full pure fact data for impact analysis)
#[derive(Debug, Serialize)]
pub struct RefStats {
    /// Total reference count
    pub total: usize,
    /// Test code references
    pub test: usize,
    /// Production code references
    pub prod: usize,
    /// Affected file count
    pub files: usize,
    /// Affected module count
    pub modules: usize,
    /// Whether the symbol is exported (public API)
    pub is_exported: bool,
}

impl From<&RefStats> for RefsSummary {
    fn from(stats: &RefStats) -> Self {
        Self {
            total: stats.total,
            test: stats.test,
            prod: stats.prod,
        }
    }
}

/// Type definition information
#[derive(Debug, Serialize)]
pub struct TypeInfo {
    pub name: String,
    pub kind: String,
    pub location: LocationOutput,
}

/// Type hierarchy item output (for supertypes/subtypes commands)
#[derive(Debug, Clone, Serialize)]
pub struct TypeHierarchyOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl TypeHierarchyOutput {
    pub fn from_item(item: &crate::models::lsp::TypeHierarchyItem, root: &std::path::Path) -> Self {
        let file = item
            .location
            .file
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| item.location.file.display().to_string());

        Self {
            name: item.name.clone(),
            kind: item.kind.to_string(),
            file,
            line: item.location.line,
            column: item.location.column,
            detail: item.detail.clone(),
        }
    }
}

/// Test information
#[derive(Debug, Serialize)]
pub struct TestInfo {
    pub name: String,
    pub location: LocationOutput,
}

/// Test coverage information (pure fact data)
#[derive(Debug, Serialize)]
pub struct TestCoverage {
    /// Test reference count
    pub count: usize,
    /// Test files referencing this symbol
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

/// Affected file info for impact analysis
#[derive(Debug, Serialize)]
pub struct AffectedFile {
    pub file: String,
    pub is_test: bool,
    pub refs: usize,
}

/// Response for impact command (pure fact data - no heuristic judgments)
#[derive(Debug, Serialize)]
pub struct ImpactResponse {
    /// Target symbol info
    pub target: TargetInfo,
    /// Reference statistics
    pub refs: RefStats,
    /// Test coverage information
    pub coverage: TestCoverage,
    /// Affected files
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<AffectedFile>,
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
