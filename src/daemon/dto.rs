//! Shared Data Transfer Objects for daemon communication
//!
//! These types are used for both serialization (server -> client)
//! and deserialization (client <- server).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::models::lsp::CallHierarchyItem;
use crate::models::symbol::{Location, Symbol, SymbolKind};

// ============================================================================
// Location DTOs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationDto {
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

impl From<&Location> for LocationDto {
    fn from(loc: &Location) -> Self {
        Self {
            file: loc.file.display().to_string(),
            line: loc.line,
            column: loc.column,
            end_line: loc.end_line,
            end_column: loc.end_column,
        }
    }
}

impl From<LocationDto> for Location {
    fn from(dto: LocationDto) -> Self {
        Location::with_end(
            PathBuf::from(dto.file),
            dto.line,
            dto.column,
            dto.end_line,
            dto.end_column,
        )
    }
}

// ============================================================================
// Symbol DTOs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolDto {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<SymbolDto>>,
}

impl SymbolDto {
    pub fn from_symbol(s: &Symbol) -> Self {
        Self {
            name: s.name.clone(),
            kind: s.kind.to_string(),
            file: s.location.file.display().to_string(),
            line: s.location.line,
            column: s.location.column,
            end_line: s.location.end_line,
            end_column: s.location.end_column,
            container: s.container.clone(),
            body: s.body.clone(),
            children: if s.children.is_empty() {
                None
            } else {
                Some(s.children.iter().map(Self::from_symbol).collect())
            },
        }
    }

    pub fn into_symbol(self) -> Symbol {
        let name = Symbol::normalize_name(
            &self.name,
            &PathBuf::from(&self.file),
            SymbolKind::from_str_loose(&self.kind),
        );

        let mut symbol = Symbol::new(
            name,
            SymbolKind::from_str_loose(&self.kind),
            Location::with_end(
                PathBuf::from(self.file),
                self.line,
                self.column,
                self.end_line,
                self.end_column,
            ),
        );

        if let Some(container) = self.container
            && !container.is_empty()
        {
            symbol = symbol.with_container(container);
        }

        if let Some(body) = self.body {
            symbol = symbol.with_body(body);
        }

        if let Some(children) = self.children {
            let child_symbols: Vec<Symbol> =
                children.into_iter().map(|c| c.into_symbol()).collect();
            symbol = symbol.with_children(child_symbols);
        }

        symbol
    }
}

// ============================================================================
// Call Hierarchy DTOs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallItemDto {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<LocationDto>,
}

impl From<&CallHierarchyItem> for CallItemDto {
    fn from(c: &CallHierarchyItem) -> Self {
        Self {
            name: c.name.clone(),
            kind: c.kind.to_string(),
            file: c.location.file.display().to_string(),
            line: c.location.line,
            column: c.location.column,
            call_site: c.call_site.as_ref().map(LocationDto::from),
        }
    }
}

// ============================================================================
// Diagnostic DTOs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticDto {
    pub message: String,
    pub severity: String,
    pub line: u32,
    pub column: u32,
}

// ============================================================================
// Signature DTOs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureDto {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    pub parameters: Vec<ParameterDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterDto {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

// ============================================================================
// Response Wrapper Types
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
pub struct SymbolsResponse {
    pub count: usize,
    pub symbols: Vec<SymbolDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReferencesResponse {
    pub count: usize,
    pub references: Vec<LocationDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DefinitionResponse {
    pub definition: Option<LocationDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HoverResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CallsResponse {
    pub count: usize,
    pub calls: Vec<CallItemDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiagnosticsResponse {
    pub count: usize,
    pub diagnostics: Vec<DiagnosticDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SignatureHelpResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signatures: Option<Vec<SignatureDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenameResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<FileChangeDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileChangeDto {
    pub file: String,
    pub edits: Vec<TextEditDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TextEditDto {
    pub range: RangeDto,
    pub new_text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RangeDto {
    pub start: PositionDto,
    pub end: PositionDto,
}

impl From<RangeDto> for crate::models::lsp::Range {
    fn from(dto: RangeDto) -> Self {
        Self::new(dto.start.into(), dto.end.into())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PositionDto {
    pub line: u32,
    pub character: u32,
}

impl From<PositionDto> for crate::models::lsp::Position {
    fn from(dto: PositionDto) -> Self {
        Self::new(dto.line, dto.character)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodeActionsResponse {
    pub actions: Vec<CodeActionDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodeActionDto {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_preferred: Option<bool>,
    pub action: serde_json::Value,
}
