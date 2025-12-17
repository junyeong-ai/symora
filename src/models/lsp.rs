//! LSP Common Types
//!
//! Single source of truth for all LSP-related types.
//! Import this module when LSP types are needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::symbol::{Location, SymbolKind};

// ============================================================================
// Core LSP Types
// ============================================================================

/// Position within a document (0-indexed, LSP standard)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    /// Convert 1-indexed CLI input to 0-indexed LSP position
    pub fn from_cli(line: u32, column: u32) -> Self {
        Self {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        }
    }

    /// Convert 0-indexed LSP position to 1-indexed display position
    pub fn to_display(&self) -> (u32, u32) {
        (self.line + 1, self.character + 1)
    }
}

/// Range within a document
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// Convert a single position to a range
    pub fn point(pos: Position) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }
}

// ============================================================================
// Text Edit Types
// ============================================================================

/// Text edit unit
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// Workspace-wide edit
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEdit {
    /// URI to TextEdit[] mapping
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<HashMap<String, Vec<TextEdit>>>,

    /// DocumentChange[] (TextDocumentEdit, CreateFile, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_changes: Option<serde_json::Value>,
}

impl WorkspaceEdit {
    /// Extract file changes from changes or document_changes
    pub fn to_file_changes(&self) -> Vec<FileChange> {
        if let Some(ref changes) = self.changes {
            changes
                .iter()
                .map(|(uri, edits)| FileChange {
                    file: uri_to_path(uri),
                    edit_count: edits.len(),
                })
                .collect()
        } else if let Some(ref doc_changes) = self.document_changes {
            doc_changes
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let text_doc = item.get("textDocument")?;
                            let uri = text_doc.get("uri")?.as_str()?;
                            let edits = item.get("edits")?.as_array()?;
                            Some(FileChange {
                                file: uri_to_path(uri),
                                edit_count: edits.len(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}

/// Per-file change summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub file: PathBuf,
    pub edit_count: usize,
}

// ============================================================================
// Hover Types
// ============================================================================

/// Hover information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoverInfo {
    pub content: String,
    /// Range of the hovered symbol
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Location>,
}

impl HoverInfo {
    /// Extract symbol name from hover content
    ///
    /// Handles markdown code blocks (```lang\n...\n```) and plain text.
    /// Parses common signature patterns to extract the identifier name.
    /// Searches through multiple code blocks to find actual symbol definitions.
    pub fn extract_symbol_name(&self) -> Option<String> {
        let content = self.content.trim();
        if content.is_empty() {
            return None;
        }

        // Try each code block in order
        if content.contains("```") {
            for block in content.split("```") {
                let block = block.trim();
                if block.is_empty() {
                    continue;
                }

                // Skip the language identifier line (e.g., "rust", "kotlin")
                let code_lines: Vec<&str> = block.lines().collect();
                for line in &code_lines {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    // Skip language identifiers (single word without spaces or "::")
                    if !line.contains(' ') && !line.contains("::") && code_lines.len() > 1 {
                        continue;
                    }
                    // Skip module paths (contain :: but no keywords)
                    if line.contains("::") && !Self::has_keyword(line) {
                        continue;
                    }
                    // Try to parse this line
                    let name = Self::parse_identifier(line);
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
            None
        } else {
            // Plain text - try first line
            let sig_line = content.lines().next()?;
            let name = Self::parse_identifier(sig_line.trim());
            if name.is_empty() { None } else { Some(name) }
        }
    }

    /// Check if line contains a keyword that indicates a symbol definition
    fn has_keyword(s: &str) -> bool {
        const KEYWORDS: &[&str] = &[
            "fn ",
            "fun ",
            "def ",
            "func ",
            "function ",
            "class ",
            "struct ",
            "enum ",
            "interface ",
            "trait ",
            "type ",
            "val ",
            "var ",
            "let ",
            "const ",
            "static ",
            "pub ",
            "public ",
            "private ",
            "protected ",
        ];
        KEYWORDS.iter().any(|kw| s.contains(kw))
    }

    /// Parse identifier from a signature line
    fn parse_identifier(sig: &str) -> String {
        // Common keywords that precede identifiers
        const KEYWORDS: &[&str] = &[
            // Functions
            "fun ",
            "fn ",
            "def ",
            "func ",
            "function ",
            // Types
            "class ",
            "struct ",
            "enum ",
            "interface ",
            "trait ",
            "type ",
            // Variables
            "val ",
            "var ",
            "let ",
            "const ",
            "static ",
            // Modifiers (skip these to get to the real keyword)
            "public ",
            "private ",
            "protected ",
            "internal ",
            "abstract ",
            "final ",
            "override ",
            "suspend ",
            "async ",
            "await ",
            "export ",
            "default ",
        ];

        let mut s = sig;

        // Strip leading modifiers (Rust: pub, Java/Kotlin: public/private, etc.)
        loop {
            let mut found = false;
            for kw in &[
                // Rust modifiers
                "pub ",
                "pub(crate) ",
                "pub(super) ",
                "pub(self) ",
                // Access modifiers
                "public ",
                "private ",
                "protected ",
                "internal ",
                // Other modifiers
                "abstract ",
                "final ",
                "override ",
                "suspend ",
                "static ",
                "async ",
                "export ",
                "default ",
                "open ",
                "sealed ",
                "inline ",
                "extern ",
                "unsafe ",
                "const ",
                "mut ",
            ] {
                if let Some(rest) = s.strip_prefix(kw) {
                    s = rest;
                    found = true;
                    break;
                }
            }
            if !found {
                break;
            }
        }

        // Find keyword and extract identifier after it
        for kw in KEYWORDS {
            if let Some(rest) = s.strip_prefix(kw) {
                return extract_name(rest);
            }
        }

        // No keyword found - try to extract first identifier-like token
        extract_name(s)
    }
}

/// Extract identifier name from start of string
fn extract_name(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

// ============================================================================
// Rename Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRenameResult {
    pub placeholder: String,
    pub range: Range,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RenameResult {
    pub changes: Vec<FileChange>,
}

// ============================================================================
// Call Hierarchy Types
// ============================================================================

/// Call hierarchy item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    /// Call site location (where the call happens)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<Location>,
}

// ============================================================================
// Type Hierarchy Types
// ============================================================================

/// Type hierarchy item (for supertypes/subtypes)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeHierarchyItem {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    /// Detail string (e.g., fully qualified name)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ============================================================================
// Inlay Hints Types
// ============================================================================

/// Inlay hint for inline type/parameter annotations
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlayHint {
    /// Position in the document
    pub position: Position,
    /// The hint label
    pub label: String,
    /// Kind of inlay hint (type, parameter, etc.)
    pub kind: InlayHintKind,
    /// Whether the hint has padding on the left
    #[serde(default)]
    pub padding_left: bool,
    /// Whether the hint has padding on the right
    #[serde(default)]
    pub padding_right: bool,
}

/// Inlay hint kind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InlayHintKind {
    /// Type annotation hint
    #[default]
    Type,
    /// Parameter hint
    Parameter,
}

impl InlayHintKind {
    pub fn from_lsp(kind: Option<u32>) -> Self {
        match kind {
            Some(1) => Self::Type,
            Some(2) => Self::Parameter,
            _ => Self::Type,
        }
    }
}

impl std::fmt::Display for InlayHintKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Type => write!(f, "type"),
            Self::Parameter => write!(f, "parameter"),
        }
    }
}

// ============================================================================
// Folding Range Types
// ============================================================================

/// Folding range for code folding
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FoldingRange {
    /// Start line (0-indexed)
    pub start_line: u32,
    /// End line (0-indexed)
    pub end_line: u32,
    /// Start character (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_character: Option<u32>,
    /// End character (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_character: Option<u32>,
    /// Folding range kind
    pub kind: FoldingRangeKind,
    /// Collapsed text to display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapsed_text: Option<String>,
}

/// Folding range kind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FoldingRangeKind {
    /// A comment block
    Comment,
    /// An import section
    Imports,
    /// A region (e.g., #region in C#)
    Region,
    /// Other (default)
    #[default]
    #[serde(other)]
    Other,
}

impl FoldingRangeKind {
    pub fn from_lsp(kind: Option<&str>) -> Self {
        match kind {
            Some("comment") => Self::Comment,
            Some("imports") => Self::Imports,
            Some("region") => Self::Region,
            _ => Self::Other,
        }
    }
}

impl std::fmt::Display for FoldingRangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Comment => write!(f, "comment"),
            Self::Imports => write!(f, "imports"),
            Self::Region => write!(f, "region"),
            Self::Other => write!(f, "other"),
        }
    }
}

// ============================================================================
// Selection Range Types
// ============================================================================

/// Selection range for smart expand/shrink selection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionRange {
    /// The range of this selection
    pub range: Range,
    /// The parent selection range (for hierarchical expansion)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Box<SelectionRange>>,
}

impl SelectionRange {
    /// Get all ranges from innermost to outermost
    pub fn to_ranges(&self) -> Vec<Range> {
        let mut ranges = vec![self.range.clone()];
        let mut current = self.parent.as_ref();
        while let Some(parent) = current {
            ranges.push(parent.range.clone());
            current = parent.parent.as_ref();
        }
        ranges
    }

    /// Get the depth of the selection hierarchy
    pub fn depth(&self) -> usize {
        let mut count = 1;
        let mut current = self.parent.as_ref();
        while let Some(parent) = current {
            count += 1;
            current = parent.parent.as_ref();
        }
        count
    }
}

// ============================================================================
// Code Lens Types
// ============================================================================

/// Code lens for inline information/actions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeLens {
    /// Range in the document
    pub range: Range,
    /// Command to execute (optional, may need resolution)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CodeLensCommand>,
    /// Data for resolution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Command associated with a code lens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeLensCommand {
    /// Display title
    pub title: String,
    /// Command identifier
    pub command: String,
    /// Command arguments
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<serde_json::Value>,
}

// ============================================================================
// Code Action Types
// ============================================================================

/// Code action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAction {
    pub title: String,
    pub kind: CodeActionKind,
    #[serde(default)]
    pub is_preferred: bool,
    /// Related diagnostics
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<WorkspaceEdit>,
    /// Original data returned by LSP server (needed for apply)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Code action kind
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CodeActionKind {
    #[serde(rename = "quickfix")]
    QuickFix,
    #[serde(rename = "refactor")]
    Refactor,
    #[serde(rename = "refactor.extract")]
    RefactorExtract,
    #[serde(rename = "refactor.inline")]
    RefactorInline,
    #[serde(rename = "refactor.rewrite")]
    RefactorRewrite,
    #[serde(rename = "source")]
    Source,
    #[serde(rename = "source.organizeImports")]
    OrganizeImports,
    #[serde(rename = "source.fixAll")]
    FixAll,
    #[serde(other)]
    #[default]
    Other,
}

impl std::fmt::Display for CodeActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QuickFix => write!(f, "quickfix"),
            Self::Refactor => write!(f, "refactor"),
            Self::RefactorExtract => write!(f, "refactor.extract"),
            Self::RefactorInline => write!(f, "refactor.inline"),
            Self::RefactorRewrite => write!(f, "refactor.rewrite"),
            Self::Source => write!(f, "source"),
            Self::OrganizeImports => write!(f, "source.organizeImports"),
            Self::FixAll => write!(f, "source.fixAll"),
            Self::Other => write!(f, "other"),
        }
    }
}

impl From<Option<&str>> for CodeActionKind {
    fn from(s: Option<&str>) -> Self {
        match s {
            Some(s) if s.starts_with("quickfix") => Self::QuickFix,
            Some("refactor.extract") => Self::RefactorExtract,
            Some("refactor.inline") => Self::RefactorInline,
            Some("refactor.rewrite") => Self::RefactorRewrite,
            Some(s) if s.starts_with("refactor") => Self::Refactor,
            Some("source.organizeImports") => Self::OrganizeImports,
            Some("source.fixAll") => Self::FixAll,
            Some(s) if s.starts_with("source") => Self::Source,
            _ => Self::Other,
        }
    }
}

/// Code action apply result
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApplyActionResult {
    pub changes: Vec<FileChangeWithEdits>,
}

/// File change with detailed edits
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChangeWithEdits {
    pub file: PathBuf,
    pub edits: Vec<TextEdit>,
}

// ============================================================================
// Signature Help Types
// ============================================================================

/// Parameter information for function signatures
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterInfo {
    /// Parameter label (name or full signature portion)
    pub label: String,
    /// Parameter documentation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

/// Function/method signature information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    /// Full signature label
    pub label: String,
    /// Signature documentation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// Parameter information
    #[serde(default)]
    pub parameters: Vec<ParameterInfo>,
    /// Currently active parameter (0-indexed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

/// Signature help result
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignatureHelp {
    /// Available signatures
    pub signatures: Vec<SignatureInfo>,
    /// Currently active signature (0-indexed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    /// Currently active parameter (0-indexed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

// ============================================================================
// Symbol Query Options
// ============================================================================

/// Options for find_symbols query
#[derive(Debug, Clone, Default)]
pub struct FindSymbolsOptions {
    /// Include source code body
    pub include_body: bool,
    /// Maximum depth for nested symbols (0 = top-level only)
    pub depth: u32,
}

impl FindSymbolsOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_body(mut self) -> Self {
        self.include_body = true;
        self
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }
}

// ============================================================================
// Server Status
// ============================================================================

/// LSP server status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Running,
    Starting,
    Stopped,
    NotInstalled { hint: Option<String> },
    NotSupported,
    Error(String),
}

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Starting => write!(f, "starting"),
            Self::Stopped => write!(f, "stopped"),
            Self::NotInstalled { .. } => write!(f, "not installed"),
            Self::NotSupported => write!(f, "not supported"),
            Self::Error(e) => write!(f, "error: {}", e),
        }
    }
}

// ============================================================================
// URI Utilities
// ============================================================================

/// Convert file path to RFC 3986 compliant file:// URI
pub fn path_to_uri(path: &Path) -> String {
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    let path_str = abs_path.to_string_lossy();
    let encoded: String = path_str
        .chars()
        .map(|c| match c {
            '/' | '.' | '-' | '_' | '~' => c.to_string(),
            c if c.is_ascii_alphanumeric() => c.to_string(),
            c => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf)
                    .bytes()
                    .map(|b| format!("%{:02X}", b))
                    .collect()
            }
        })
        .collect();

    format!("file://{encoded}")
}

/// Convert file:// URI to PathBuf with full percent-decoding
pub fn uri_to_path(uri: &str) -> PathBuf {
    let path = match uri.strip_prefix("file://") {
        Some(p) => p,
        None => {
            tracing::warn!("Invalid file URI (missing file:// prefix): {}", uri);
            return PathBuf::from(uri);
        }
    };

    // Windows: file:///C:/path → C:/path (strip leading /)
    #[cfg(windows)]
    let path = path.strip_prefix('/').unwrap_or(path);

    PathBuf::from(percent_decode(path))
}

fn percent_decode(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    let mut chars = input.bytes().peekable();

    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let high = chars.next().and_then(hex_value);
            let low = chars.next().and_then(hex_value);
            if let (Some(h), Some(l)) = (high, low) {
                result.push((h << 4) | l);
                continue;
            }
        }
        result.push(byte);
    }

    String::from_utf8_lossy(&result).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_from_cli() {
        let pos = Position::from_cli(10, 5);
        assert_eq!(pos.line, 9);
        assert_eq!(pos.character, 4);
    }

    #[test]
    fn test_position_to_display() {
        let pos = Position::new(9, 4);
        assert_eq!(pos.to_display(), (10, 5));
    }

    #[test]
    fn test_workspace_edit_to_file_changes() {
        let mut changes = HashMap::new();
        changes.insert(
            "file:///test.rs".to_string(),
            vec![TextEdit {
                range: Range::default(),
                new_text: "new".to_string(),
            }],
        );

        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
        };

        let file_changes = edit.to_file_changes();
        assert_eq!(file_changes.len(), 1);
        assert_eq!(file_changes[0].edit_count, 1);
    }

    #[test]
    fn test_uri_roundtrip_simple() {
        let path = PathBuf::from("/test/file.rs");
        let uri = path_to_uri(&path);
        let back = uri_to_path(&uri);
        assert_eq!(back, path);
    }

    #[test]
    fn test_uri_with_spaces() {
        let path = PathBuf::from("/path with spaces/file.rs");
        let uri = path_to_uri(&path);
        assert!(uri.contains("%20"));
        let back = uri_to_path(&uri);
        assert_eq!(back, path);
    }

    #[test]
    fn test_uri_with_unicode() {
        let path = PathBuf::from("/tmp/한글_테스트.rs");
        let uri = path_to_uri(&path);
        let back = uri_to_path(&uri);
        assert_eq!(back, path);
    }

    #[test]
    fn test_percent_decode() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("test%2Fpath"), "test/path");
        assert_eq!(percent_decode("normal"), "normal");
    }

    #[test]
    fn test_code_action_kind_display() {
        assert_eq!(CodeActionKind::QuickFix.to_string(), "quickfix");
        assert_eq!(
            CodeActionKind::OrganizeImports.to_string(),
            "source.organizeImports"
        );
    }

    #[test]
    fn test_hover_extract_symbol_name_kotlin() {
        let hover = HoverInfo {
            content: "```kotlin\nfun toDomain(): Order\n```".to_string(),
            range: None,
        };
        assert_eq!(hover.extract_symbol_name(), Some("toDomain".to_string()));
    }

    #[test]
    fn test_hover_extract_symbol_name_rust() {
        let hover = HoverInfo {
            content: "```rust\npub fn new() -> Self\n```".to_string(),
            range: None,
        };
        assert_eq!(hover.extract_symbol_name(), Some("new".to_string()));
    }

    #[test]
    fn test_hover_extract_symbol_name_python() {
        let hover = HoverInfo {
            content: "def my_function(x: int) -> str".to_string(),
            range: None,
        };
        assert_eq!(hover.extract_symbol_name(), Some("my_function".to_string()));
    }

    #[test]
    fn test_hover_extract_symbol_name_class() {
        let hover = HoverInfo {
            content: "public class MyClass".to_string(),
            range: None,
        };
        assert_eq!(hover.extract_symbol_name(), Some("MyClass".to_string()));
    }

    #[test]
    fn test_hover_extract_symbol_name_empty() {
        let hover = HoverInfo {
            content: "".to_string(),
            range: None,
        };
        assert_eq!(hover.extract_symbol_name(), None);
    }

    #[test]
    fn test_hover_extract_symbol_name_rust_with_module_path() {
        // Rust hover often includes module path in first code block
        let hover = HoverInfo {
            content: "```rust\ndata_collector_rs::models::location\n```\n\n```rust\npub struct LocationData {\n    pub ts: i64,\n}\n```".to_string(),
            range: None,
        };
        assert_eq!(
            hover.extract_symbol_name(),
            Some("LocationData".to_string())
        );
    }

    #[test]
    fn test_hover_extract_symbol_name_rust_function_with_module() {
        let hover = HoverInfo {
            content: "```rust\nmy_crate::utils\n```\n\n```rust\npub fn process_data(input: &str) -> Result<(), Error>\n```".to_string(),
            range: None,
        };
        assert_eq!(
            hover.extract_symbol_name(),
            Some("process_data".to_string())
        );
    }
}
