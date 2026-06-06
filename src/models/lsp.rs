use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::symbol::{Location, SymbolKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// Why a result set may be an incomplete lower bound.
///
/// Surfaced on `refs` / `callers` / `callees` / `impact` and on semantic
/// search so an agent can distinguish "few results" from "not everything
/// was searched". Absent = complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexingDegradation {
    /// The workspace-indexing wait hit its budget; results are a lower
    /// bound. Retrying after the server warms up may return more.
    TimedOut,
    /// Only part of the corpus was searched before a size cap was hit
    /// (e.g. semantic search over a very large repo); results are a lower
    /// bound, not a complete ranking.
    Capped,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEdit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<HashMap<String, Vec<TextEdit>>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_changes: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoverInfo {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Location>,
}

impl HoverInfo {
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

    fn has_keyword(s: &str) -> bool {
        DEFINITION_KEYWORDS.iter().any(|kw| s.contains(kw))
            || MODIFIER_KEYWORDS.iter().any(|kw| s.contains(kw))
    }

    fn parse_identifier(sig: &str) -> String {
        let mut s = sig;

        // Strip leading modifiers
        loop {
            let mut found = false;
            for kw in MODIFIER_KEYWORDS {
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
        for kw in DEFINITION_KEYWORDS {
            if let Some(rest) = s.strip_prefix(kw) {
                return extract_name(rest);
            }
        }

        // No keyword found - try to extract first identifier-like token
        extract_name(s)
    }
}

const DEFINITION_KEYWORDS: &[&str] = &[
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
];

const MODIFIER_KEYWORDS: &[&str] = &[
    "pub ",
    "pub(crate) ",
    "pub(super) ",
    "pub(self) ",
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
    "open ",
    "sealed ",
    "inline ",
    "extern ",
    "unsafe ",
    "mut ",
];

fn extract_name(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRenameResult {
    pub placeholder: String,
    pub range: Range,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RenameResult {
    pub changes: Vec<FileChangeWithEdits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<Location>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeHierarchyItem {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlayHint {
    pub position: Position,
    pub label: String,
    pub kind: InlayHintKind,
    #[serde(default)]
    pub padding_left: bool,
    #[serde(default)]
    pub padding_right: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InlayHintKind {
    #[default]
    Type,
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

    pub fn to_lsp(&self) -> u32 {
        match self {
            Self::Type => 1,
            Self::Parameter => 2,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FoldingRange {
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_character: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_character: Option<u32>,
    pub kind: FoldingRangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapsed_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FoldingRangeKind {
    Comment,
    Imports,
    Region,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionRange {
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Box<SelectionRange>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeLens {
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CodeLensCommand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeLensCommand {
    pub title: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAction {
    pub title: String,
    pub kind: CodeActionKind,
    #[serde(default)]
    pub is_preferred: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<WorkspaceEdit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApplyActionResult {
    pub changes: Vec<FileChangeWithEdits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChangeWithEdits {
    pub file: PathBuf,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterInfo {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    #[serde(default)]
    pub parameters: Vec<ParameterInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignatureHelp {
    pub signatures: Vec<SignatureInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct FindSymbolsOptions {
    pub include_body: bool,
    pub depth: u32,
}

impl Default for FindSymbolsOptions {
    fn default() -> Self {
        Self {
            include_body: false,
            depth: u32::MAX,
        }
    }
}

impl FindSymbolsOptions {
    pub fn with_body(mut self) -> Self {
        self.include_body = true;
        self
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Running,
    Stopped,
    NotInstalled { hint: Option<String> },
    NotSupported,
}

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Stopped => write!(f, "stopped"),
            Self::NotInstalled { .. } => write!(f, "not installed"),
            Self::NotSupported => write!(f, "not supported"),
        }
    }
}

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
            '/' | ':' | '.' | '-' | '_' | '~' => c.to_string(),
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
    let bytes = input.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            result.push((h << 4) | l);
            i += 3;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Malformed sequences: bytes should be preserved, not lost
        assert_eq!(percent_decode("%GG"), "%GG");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%"), "a%");
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
