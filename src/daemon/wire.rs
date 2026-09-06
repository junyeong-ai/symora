use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::models::lsp::{CodeActionKind, FoldingRangeKind, InlayHintKind};
use crate::models::symbol::SymbolKind;
use crate::models::{diagnostic, lsp, symbol};
use crate::services::store::UnreadPath;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_end_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_start_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_column: Option<bool>,
}

impl From<&symbol::Location> for Location {
    fn from(loc: &symbol::Location) -> Self {
        Self {
            file: loc.file.display().to_string(),
            line: loc.line,
            column: loc.column,
            name_end_line: loc.name_end_line,
            name_end_column: loc.name_end_column,
            range_start_line: loc.range_start_line,
            range_start_column: loc.range_start_column,
            end_line: loc.end_line,
            end_column: loc.end_column,
            // Kept in lockstep across the wire so a degraded column discloses
            // itself identically in daemon and in-process mode (invariant 3).
            degraded_column: loc.degraded_column,
        }
    }
}

impl From<Location> for symbol::Location {
    fn from(val: Location) -> Self {
        Self {
            file: PathBuf::from(val.file),
            line: val.line,
            column: val.column,
            name_end_line: val.name_end_line,
            name_end_column: val.name_end_column,
            range_start_line: val.range_start_line,
            range_start_column: val.range_start_column,
            end_line: val.end_line,
            end_column: val.end_column,
            degraded_column: val.degraded_column,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_end_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_start_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    /// Disclosed degraded column. `find_symbols` (single seeded file) never
    /// degrades, but `workspace/symbol` returns cross-file results decoded
    /// against possibly-unreadable files, so the flag must survive the wire to
    /// keep daemon and in-process workspace-symbol output in agreement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_column: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Symbol>>,
}

impl From<&symbol::Symbol> for Symbol {
    fn from(s: &symbol::Symbol) -> Self {
        Self {
            name: s.name.clone(),
            kind: s.kind.to_string(),
            file: s.location.file.display().to_string(),
            line: s.location.line,
            column: s.location.column,
            name_end_line: s.location.name_end_line,
            name_end_column: s.location.name_end_column,
            range_start_line: s.location.range_start_line,
            range_start_column: s.location.range_start_column,
            end_line: s.location.end_line,
            end_column: s.location.end_column,
            degraded_column: s.location.degraded_column,
            container: s.container.clone(),
            body: s.body.clone(),
            children: if s.children.is_empty() {
                None
            } else {
                Some(s.children.iter().map(Self::from).collect())
            },
        }
    }
}

impl From<Symbol> for symbol::Symbol {
    fn from(val: Symbol) -> Self {
        let kind = SymbolKind::parse_or_default(&val.kind);
        let name = symbol::Symbol::normalize_name(&val.name, &PathBuf::from(&val.file), kind);

        let location = symbol::Location {
            file: PathBuf::from(val.file),
            line: val.line,
            column: val.column,
            name_end_line: val.name_end_line,
            name_end_column: val.name_end_column,
            range_start_line: val.range_start_line,
            range_start_column: val.range_start_column,
            end_line: val.end_line,
            end_column: val.end_column,
            degraded_column: val.degraded_column,
        };

        let mut sym = symbol::Symbol::new(name, kind, location);

        if let Some(container) = val.container
            && !container.is_empty()
        {
            sym = sym.with_container(container);
        }

        if let Some(body) = val.body {
            sym = sym.with_body(body);
        }

        if let Some(children) = val.children {
            sym = sym.with_children(children.into_iter().map(symbol::Symbol::from).collect());
        }

        sym
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallItem {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// Disclosed degraded column for the call target, kept across the wire so
    /// daemon and in-process callers/callees agree (invariant 3). The target is
    /// a cross-file result that can degrade; `call_site` carries its own flag
    /// via `wire::Location`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_column: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<Location>,
}

impl From<&lsp::CallHierarchyItem> for CallItem {
    fn from(c: &lsp::CallHierarchyItem) -> Self {
        Self {
            name: c.name.clone(),
            kind: c.kind.to_string(),
            file: c.location.file.display().to_string(),
            line: c.location.line,
            column: c.location.column,
            degraded_column: c.location.degraded_column,
            call_site: c.call_site.as_ref().map(Location::from),
        }
    }
}

impl From<CallItem> for lsp::CallHierarchyItem {
    fn from(val: CallItem) -> Self {
        Self {
            name: val.name,
            kind: SymbolKind::parse_or_default(&val.kind),
            location: symbol::Location::point(PathBuf::from(&val.file), val.line, val.column)
                .with_degraded_column(val.degraded_column == Some(true)),
            call_site: val.call_site.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub message: String,
    pub severity: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_information: Vec<RelatedInformation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedInformation {
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// Disclosed degraded column for this related location, kept across the
    /// wire so daemon and in-process diagnostics agree (invariant 3) — related
    /// info points at cross-file locations that can degrade on an unreadable line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_column: Option<bool>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    pub parameters: Vec<Parameter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameter {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl From<Range> for lsp::Range {
    fn from(val: Range) -> Self {
        Self::new(val.start.into(), val.end.into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl From<Position> for lsp::Position {
    fn from(val: Position) -> Self {
        Self::new(val.line, val.character)
    }
}

impl From<&lsp::Position> for Position {
    fn from(p: &lsp::Position) -> Self {
        Self {
            line: p.line,
            character: p.character,
        }
    }
}

impl From<&lsp::Range> for Range {
    fn from(r: &lsp::Range) -> Self {
        Self {
            start: Position::from(&r.start),
            end: Position::from(&r.end),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub file: String,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

impl From<TextEdit> for lsp::TextEdit {
    fn from(val: TextEdit) -> Self {
        Self {
            range: val.range.into(),
            new_text: val.new_text,
        }
    }
}

impl From<&lsp::FileChangeWithEdits> for FileChange {
    fn from(c: &lsp::FileChangeWithEdits) -> Self {
        Self {
            file: c.file.display().to_string(),
            edits: c
                .edits
                .iter()
                .map(|e| TextEdit {
                    range: Range::from(&e.range),
                    new_text: e.new_text.clone(),
                })
                .collect(),
        }
    }
}

impl From<FileChange> for lsp::FileChangeWithEdits {
    fn from(val: FileChange) -> Self {
        Self {
            file: PathBuf::from(val.file),
            edits: val.edits.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<Parameter> for lsp::ParameterInfo {
    fn from(val: Parameter) -> Self {
        Self {
            label: val.label,
            documentation: val.documentation,
        }
    }
}

impl From<Signature> for lsp::SignatureInfo {
    fn from(val: Signature) -> Self {
        Self {
            label: val.label,
            documentation: val.documentation,
            parameters: val.parameters.into_iter().map(Into::into).collect(),
            active_parameter: val.active_parameter,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SymbolsResponse {
    pub count: usize,
    pub symbols: Vec<Symbol>,
    /// Computation-time indexing snapshot (workspace-symbol responses
    /// only; `find_symbols` is a single-document query and leaves it
    /// absent). See `models::lsp::Indexed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReferencesResponse {
    pub count: usize,
    pub references: Vec<Location>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

/// A store search's payload: rows plus the coverage they were read under,
/// from one store snapshot — the wire form of `store::SearchPage`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResponse<T> {
    pub count: usize,
    pub items: Vec<T>,
    /// The files backing `items` that changed since they were indexed. Named
    /// rather than a flag because a caller that filters the page narrows the
    /// question to the files its answer kept — a distinction the direct path
    /// makes, so the wire has to carry it too.
    #[serde(default, skip_serializing_if = "<[String]>::is_empty")]
    pub stale_files: Vec<String>,
    #[serde(default, skip_serializing_if = "<[String]>::is_empty")]
    pub covered: Vec<String>,
    /// Paths the build behind this page could not read. They qualify
    /// `covered` — the languages named there are indexed only in part while
    /// any remain — so they have to cross with them, or the daemon path
    /// would vouch for a coverage the direct path knows is partial.
    #[serde(default, skip_serializing_if = "<[UnreadPath]>::is_empty")]
    pub unread_paths: Vec<UnreadPath>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DefinitionResponse {
    pub definition: Option<Location>,
    /// Whether the server reported the queried token as its own definition
    /// (see [`lsp::Definition`]); always false for type definitions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_self: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HoverResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Location>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImplementationsResponse {
    pub count: usize,
    pub implementations: Vec<Location>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CallsResponse {
    pub count: usize,
    pub calls: Vec<CallItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiagnosticsResponse {
    pub status: diagnostic::DiagnosticsStatus,
    pub count: usize,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SignatureResponse {
    pub signatures: Vec<Signature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrepareRenameResponse {
    pub placeholder: Option<String>,
}

/// `changes` is `None` when the server declined the rename — nothing
/// renameable at the position — and the applied edit set otherwise.
#[derive(Debug, Serialize, Deserialize)]
pub struct RenameResponse {
    pub changes: Option<Vec<FileChange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodeActionsResponse {
    pub count: usize,
    pub actions: Vec<CodeAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAction {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default)]
    pub is_preferred: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApplyActionResponse {
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FormatResponse {
    pub count: usize,
    pub edits: Vec<FormatEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatEdit {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub new_text: String,
}

impl From<&lsp::TextEdit> for FormatEdit {
    fn from(edit: &lsp::TextEdit) -> Self {
        Self {
            start_line: edit.range.start.line,
            start_character: edit.range.start.character,
            end_line: edit.range.end.line,
            end_character: edit.range.end.character,
            new_text: edit.new_text.clone(),
        }
    }
}

impl From<FormatEdit> for lsp::TextEdit {
    fn from(edit: FormatEdit) -> Self {
        Self {
            range: lsp::Range::new(
                lsp::Position::new(edit.start_line, edit.start_character),
                lsp::Position::new(edit.end_line, edit.end_character),
            ),
            new_text: edit.new_text,
        }
    }
}

impl From<Vec<lsp::TextEdit>> for FormatResponse {
    fn from(edits: Vec<lsp::TextEdit>) -> Self {
        Self {
            count: edits.len(),
            edits: edits.iter().map(FormatEdit::from).collect(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TypeHierarchyResponse {
    pub count: usize,
    pub items: Vec<TypeHierarchyItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<lsp::IndexingDegradation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeHierarchyItem {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// Disclosed degraded column, kept across the wire so daemon and in-process
    /// supertypes/subtypes agree (invariant 3) — a type hierarchy item is a
    /// cross-file result that can degrade on an unreadable line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_column: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl From<&lsp::TypeHierarchyItem> for TypeHierarchyItem {
    fn from(item: &lsp::TypeHierarchyItem) -> Self {
        Self {
            name: item.name.clone(),
            kind: item.kind.to_string(),
            file: item.location.file.display().to_string(),
            line: item.location.line,
            column: item.location.column,
            degraded_column: item.location.degraded_column,
            detail: item.detail.clone(),
        }
    }
}

impl From<TypeHierarchyItem> for lsp::TypeHierarchyItem {
    fn from(val: TypeHierarchyItem) -> Self {
        Self {
            name: val.name,
            kind: SymbolKind::parse_or_default(&val.kind),
            location: symbol::Location::point(PathBuf::from(val.file), val.line, val.column)
                .with_degraded_column(val.degraded_column == Some(true)),
            detail: val.detail,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InlayHintsResponse {
    pub count: usize,
    pub hints: Vec<InlayHint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHint {
    pub line: u32,
    pub character: u32,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<u32>,
    #[serde(default)]
    pub padding_left: bool,
    #[serde(default)]
    pub padding_right: bool,
}

impl From<InlayHint> for lsp::InlayHint {
    fn from(val: InlayHint) -> Self {
        Self {
            position: lsp::Position::new(val.line, val.character),
            label: val.label,
            kind: InlayHintKind::from_lsp(val.kind),
            padding_left: val.padding_left,
            padding_right: val.padding_right,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FoldingRangesResponse {
    pub count: usize,
    pub ranges: Vec<FoldingRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldingRange {
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_character: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_character: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapsed_text: Option<String>,
}

impl From<FoldingRange> for lsp::FoldingRange {
    fn from(val: FoldingRange) -> Self {
        Self {
            start_line: val.start_line,
            end_line: val.end_line,
            start_character: val.start_character,
            end_character: val.end_character,
            kind: FoldingRangeKind::from_lsp(val.kind.as_deref()),
            collapsed_text: val.collapsed_text,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SelectionRangesResponse {
    pub count: usize,
    pub ranges: Vec<SelectionRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionRange {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Box<SelectionRange>>,
}

impl From<SelectionRange> for lsp::SelectionRange {
    fn from(val: SelectionRange) -> Self {
        Self {
            range: lsp::Range::new(
                lsp::Position::new(val.start_line, val.start_character),
                lsp::Position::new(val.end_line, val.end_character),
            ),
            parent: val.parent.map(|p| Box::new((*p).into())),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodeLensResponse {
    pub count: usize,
    pub lenses: Vec<CodeLens>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeLens {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
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

impl From<CodeLens> for lsp::CodeLens {
    fn from(val: CodeLens) -> Self {
        Self {
            range: lsp::Range::new(
                lsp::Position::new(val.start_line, val.start_character),
                lsp::Position::new(val.end_line, val.end_character),
            ),
            command: val.command.map(|cmd| lsp::CodeLensCommand {
                title: cmd.title,
                command: cmd.command,
                arguments: cmd.arguments,
            }),
            data: val.data,
        }
    }
}

impl From<CodeAction> for lsp::CodeAction {
    fn from(val: CodeAction) -> Self {
        Self {
            title: val.title,
            kind: CodeActionKind::from(val.kind.as_deref()),
            is_preferred: val.is_preferred,
            diagnostics: val.diagnostics,
            edit: None,
            data: val.data,
        }
    }
}

// Response builder From impls for dispatch simplification

macro_rules! impl_vec_response {
    ($resp:ty, $field:ident, $domain:ty, $wire:ty) => {
        impl From<lsp::Indexed<Vec<$domain>>> for $resp {
            fn from(result: lsp::Indexed<Vec<$domain>>) -> Self {
                Self {
                    count: result.data.len(),
                    $field: result.data.iter().map(<$wire>::from).collect(),
                    indexing: result.indexing,
                }
            }
        }
    };
}

impl_vec_response!(ReferencesResponse, references, symbol::Location, Location);
impl_vec_response!(
    ImplementationsResponse,
    implementations,
    symbol::Location,
    Location
);
impl_vec_response!(CallsResponse, calls, lsp::CallHierarchyItem, CallItem);
impl_vec_response!(
    TypeHierarchyResponse,
    items,
    lsp::TypeHierarchyItem,
    TypeHierarchyItem
);

impl DefinitionResponse {
    pub fn from_definition(def: lsp::Indexed<Option<lsp::Definition>>) -> Self {
        let message = def
            .data
            .is_none()
            .then(|| "No definition found".to_string());
        Self {
            is_self: def.data.as_ref().is_some_and(|d| d.is_self),
            definition: def.data.as_ref().map(|d| Location::from(&d.location)),
            message,
            indexing: def.indexing,
        }
    }

    pub fn from_type_definition(def: lsp::Indexed<Option<symbol::Location>>) -> Self {
        Self {
            definition: def.data.as_ref().map(Location::from),
            is_self: false,
            message: def
                .data
                .is_none()
                .then(|| "No type definition found".to_string()),
            indexing: def.indexing,
        }
    }
}

impl HoverResponse {
    pub fn from_hover(hover: lsp::Indexed<Option<lsp::HoverInfo>>) -> Self {
        Self {
            content: hover.data.as_ref().map(|h| h.content.clone()),
            range: hover
                .data
                .as_ref()
                .and_then(|h| h.range.as_ref().map(Location::from)),
            message: if hover.data.is_none() {
                Some("No hover information".into())
            } else {
                None
            },
            indexing: hover.indexing,
        }
    }
}

impl SignatureResponse {
    pub fn from_help(help: lsp::Indexed<Option<lsp::SignatureHelp>>) -> Self {
        let indexing = help.indexing;
        match help.data {
            Some(h) => Self {
                signatures: h
                    .signatures
                    .iter()
                    .map(|s| Signature {
                        label: s.label.clone(),
                        documentation: s.documentation.clone(),
                        parameters: s
                            .parameters
                            .iter()
                            .map(|p| Parameter {
                                label: p.label.clone(),
                                documentation: p.documentation.clone(),
                            })
                            .collect(),
                        active_parameter: s.active_parameter,
                    })
                    .collect(),
                active_signature: h.active_signature,
                active_parameter: h.active_parameter,
                message: None,
                indexing,
            },
            None => Self {
                signatures: vec![],
                active_signature: None,
                active_parameter: None,
                message: Some("No signature help available".into()),
                indexing,
            },
        }
    }
}

impl From<diagnostic::DiagnosticsReport> for DiagnosticsResponse {
    fn from(report: diagnostic::DiagnosticsReport) -> Self {
        Self {
            status: report.status,
            count: report.items.len(),
            diagnostics: report
                .items
                .iter()
                .map(|d| Diagnostic {
                    message: d.message.clone(),
                    severity: d.severity.to_string(),
                    line: d.display_line(),
                    column: d.display_column(),
                    end_line: d.display_end_line(),
                    end_column: d.display_end_column(),
                    source: d.source.clone(),
                    code: d.code.clone(),
                    tags: d.tags.iter().map(|t| t.to_string()).collect(),
                    related_information: d
                        .related_information
                        .iter()
                        .map(|ri| RelatedInformation {
                            file: ri.location.file.display().to_string(),
                            line: ri.location.line,
                            column: ri.location.column,
                            degraded_column: ri.location.degraded_column,
                            message: ri.message.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<Vec<lsp::FoldingRange>> for FoldingRangesResponse {
    fn from(ranges: Vec<lsp::FoldingRange>) -> Self {
        Self {
            count: ranges.len(),
            ranges: ranges
                .iter()
                .map(|r| FoldingRange {
                    start_line: r.start_line,
                    end_line: r.end_line,
                    start_character: r.start_character,
                    end_character: r.end_character,
                    kind: Some(r.kind.to_string()),
                    collapsed_text: r.collapsed_text.clone(),
                })
                .collect(),
        }
    }
}

impl From<Vec<lsp::CodeLens>> for CodeLensResponse {
    fn from(lenses: Vec<lsp::CodeLens>) -> Self {
        Self {
            count: lenses.len(),
            lenses: lenses
                .iter()
                .map(|l| CodeLens {
                    start_line: l.range.start.line,
                    start_character: l.range.start.character,
                    end_line: l.range.end.line,
                    end_character: l.range.end.character,
                    command: l.command.as_ref().map(|c| CodeLensCommand {
                        title: c.title.clone(),
                        command: c.command.clone(),
                        arguments: c.arguments.clone(),
                    }),
                    data: l.data.clone(),
                })
                .collect(),
        }
    }
}

impl CodeActionsResponse {
    pub fn from_actions(actions: Vec<lsp::CodeAction>) -> Self {
        Self {
            count: actions.len(),
            actions: actions
                .iter()
                .map(|a| CodeAction {
                    title: a.title.clone(),
                    kind: Some(a.kind.to_string()),
                    is_preferred: a.is_preferred,
                    diagnostics: a.diagnostics.clone(),
                    data: a.data.clone(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::lsp::{CallHierarchyItem as LspCallItem, TypeHierarchyItem as LspTypeItem};
    use crate::models::symbol::{Location as SymLocation, Symbol as SymSymbol, SymbolKind};

    /// The computation-time indexing marker must survive the daemon wire
    /// (INV3: daemon and direct answers carry the same disclosure), and a
    /// complete answer must omit the field rather than ship filler.
    #[test]
    fn indexing_marker_round_trips_and_omits_when_absent() {
        let degraded = ReferencesResponse::from(lsp::Indexed::new(
            vec![SymLocation::point(PathBuf::from("src/a.rs"), 3, 7)],
            Some(lsp::IndexingDegradation::TimedOut),
        ));
        let wire = serde_json::to_value(&degraded).unwrap();
        assert_eq!(wire["indexing"], "timed_out");
        let back: ReferencesResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(back.indexing, Some(lsp::IndexingDegradation::TimedOut));
        assert_eq!(back.count, 1);

        let complete = ReferencesResponse::from(lsp::Indexed::complete(Vec::<SymLocation>::new()));
        let wire = serde_json::to_value(&complete).unwrap();
        assert!(wire.get("indexing").is_none());
        let back: ReferencesResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(back.indexing, None);
    }

    // ---------------------------------------------------------------
    // Location roundtrip tests
    // ---------------------------------------------------------------

    #[test]
    fn location_roundtrip_full_fields() {
        let original = SymLocation {
            file: PathBuf::from("src/main.rs"),
            line: 10,
            column: 5,
            name_end_line: Some(10),
            name_end_column: Some(9),
            range_start_line: Some(10),
            range_start_column: Some(1),
            end_line: Some(15),
            end_column: Some(2),
            // A degraded column must survive the wire so daemon and in-process
            // disclose it identically (invariant 3) — the round-trip proves it.
            degraded_column: Some(true),
        };

        let wire = Location::from(&original);
        let back: SymLocation = wire.into();

        assert_eq!(back.file, original.file);
        assert_eq!(back.line, original.line);
        assert_eq!(back.column, original.column);
        assert_eq!(back.name_end_line, original.name_end_line);
        assert_eq!(back.name_end_column, original.name_end_column);
        assert_eq!(back.range_start_line, original.range_start_line);
        assert_eq!(back.range_start_column, original.range_start_column);
        assert_eq!(back.end_line, original.end_line);
        assert_eq!(back.end_column, original.end_column);
        assert_eq!(back.degraded_column, Some(true));
    }

    #[test]
    fn location_roundtrip_minimal() {
        let original = SymLocation::point(PathBuf::from("test.rs"), 1, 1);

        let wire = Location::from(&original);
        let back: SymLocation = wire.into();

        assert_eq!(back.file, original.file);
        assert_eq!(back.line, original.line);
        assert_eq!(back.column, original.column);
        assert_eq!(back.range_start_line, None);
        assert_eq!(back.range_start_column, None);
        assert_eq!(back.end_line, None);
        assert_eq!(back.end_column, None);
    }

    #[test]
    fn location_roundtrip_preserves_path_string() {
        let original = SymLocation::point(PathBuf::from("/absolute/path/to/file.rs"), 42, 13);

        let wire = Location::from(&original);
        assert_eq!(wire.file, "/absolute/path/to/file.rs");

        let back: SymLocation = wire.into();
        assert_eq!(back.file, original.file);
    }

    // ---------------------------------------------------------------
    // Symbol roundtrip tests
    // ---------------------------------------------------------------

    #[test]
    fn symbol_roundtrip_minimal() {
        let loc = SymLocation::point(PathBuf::from("test.rs"), 5, 3);
        let original = SymSymbol::new("my_function".to_string(), SymbolKind::Function, loc);

        let wire = Symbol::from(&original);
        let back: SymSymbol = wire.into();

        assert_eq!(back.name, original.name);
        assert_eq!(back.kind, original.kind);
        assert_eq!(back.location.file, original.location.file);
        assert_eq!(back.location.line, original.location.line);
        assert_eq!(back.location.column, original.location.column);
        assert_eq!(back.container, None);
        assert_eq!(back.body, None);
        assert!(back.children.is_empty());
    }

    #[test]
    fn symbol_roundtrip_with_container() {
        let loc = SymLocation {
            file: PathBuf::from("src/lib.rs"),
            line: 20,
            column: 8,
            name_end_line: Some(20),
            name_end_column: Some(14),
            range_start_line: Some(18),
            range_start_column: Some(1),
            end_line: Some(30),
            end_column: Some(2),
            degraded_column: None,
        };
        let original = SymSymbol::new("update".to_string(), SymbolKind::Method, loc)
            .with_container("MyStruct".to_string());

        let wire = Symbol::from(&original);
        let back: SymSymbol = wire.into();

        assert_eq!(back.name, "update");
        assert_eq!(back.kind, SymbolKind::Method);
        assert_eq!(back.container, Some("MyStruct".to_string()));
        assert_eq!(back.location.name_end_column, Some(14));
        assert_eq!(back.location.range_start_line, Some(18));
        assert_eq!(back.location.end_line, Some(30));
    }

    #[test]
    fn symbol_roundtrip_with_body() {
        let loc = SymLocation::point(PathBuf::from("test.rs"), 1, 1);
        let original = SymSymbol::new("greet".to_string(), SymbolKind::Function, loc)
            .with_body("fn greet() { println!(\"hello\"); }".to_string());

        let wire = Symbol::from(&original);
        let back: SymSymbol = wire.into();

        assert_eq!(back.body, original.body);
    }

    #[test]
    fn symbol_roundtrip_with_children() {
        let parent_loc = SymLocation::point(PathBuf::from("test.rs"), 1, 1);
        let child_loc = SymLocation::point(PathBuf::from("test.rs"), 5, 5);

        let child = SymSymbol::new("inner".to_string(), SymbolKind::Method, child_loc);
        let original = SymSymbol::new("Outer".to_string(), SymbolKind::Class, parent_loc)
            .with_children(vec![child]);

        let wire = Symbol::from(&original);

        // Verify wire has children
        assert!(wire.children.is_some());
        assert_eq!(wire.children.as_ref().unwrap().len(), 1);

        let back: SymSymbol = wire.into();

        assert_eq!(back.name, "Outer");
        assert_eq!(back.kind, SymbolKind::Class);
        assert_eq!(back.children.len(), 1);
        assert_eq!(back.children[0].name, "inner");
        assert_eq!(back.children[0].kind, SymbolKind::Method);
    }

    #[test]
    fn symbol_roundtrip_preserves_a_degraded_workspace_column() {
        // A `workspace/symbol` result is cross-file and can be decoded against
        // an unreadable line; the degraded flag must survive the wire so daemon
        // and in-process workspace-symbol output agree (invariant 3).
        let loc =
            SymLocation::point(PathBuf::from("src/other.rs"), 9, 2).with_degraded_column(true);
        let original = SymSymbol::new("Widget".to_string(), SymbolKind::Struct, loc);

        let wire = Symbol::from(&original);
        assert_eq!(wire.degraded_column, Some(true));
        let back: SymSymbol = wire.into();
        assert_eq!(back.location.degraded_column, Some(true));
    }

    #[test]
    fn symbol_roundtrip_kind_survives_string_conversion() {
        // Verify all commonly used SymbolKind values survive the to_string -> parse_or_default roundtrip
        let kinds = [
            SymbolKind::Function,
            SymbolKind::Class,
            SymbolKind::Method,
            SymbolKind::Field,
            SymbolKind::Struct,
            SymbolKind::Enum,
            SymbolKind::Interface,
            SymbolKind::Module,
            SymbolKind::Property,
            SymbolKind::Constructor,
            SymbolKind::Variable,
            SymbolKind::Constant,
            SymbolKind::EnumMember,
            SymbolKind::TypeParameter,
        ];

        for kind in kinds {
            let loc = SymLocation::point(PathBuf::from("test.rs"), 1, 1);
            let original = SymSymbol::new("sym".to_string(), kind, loc);
            let wire = Symbol::from(&original);
            let back: SymSymbol = wire.into();
            assert_eq!(
                back.kind, kind,
                "SymbolKind {:?} did not survive roundtrip",
                kind
            );
        }
    }

    #[test]
    fn symbol_roundtrip_no_children_becomes_none_in_wire() {
        let loc = SymLocation::point(PathBuf::from("test.rs"), 1, 1);
        let original = SymSymbol::new("lonely".to_string(), SymbolKind::Function, loc);

        let wire = Symbol::from(&original);
        assert!(
            wire.children.is_none(),
            "empty children should serialize as None"
        );

        let back: SymSymbol = wire.into();
        assert!(back.children.is_empty());
    }

    // ---------------------------------------------------------------
    // CallHierarchyItem roundtrip tests
    // ---------------------------------------------------------------

    #[test]
    fn call_item_roundtrip_with_call_site() {
        let loc = SymLocation {
            file: PathBuf::from("src/service.rs"),
            line: 25,
            column: 10,
            name_end_line: None,
            name_end_column: None,
            range_start_line: Some(25),
            range_start_column: Some(1),
            end_line: Some(40),
            end_column: Some(2),
            // A degraded column on the call target must survive the wire so
            // daemon and in-process callers/callees disclose it identically.
            degraded_column: Some(true),
        };
        let call_site = SymLocation {
            file: PathBuf::from("src/handler.rs"),
            line: 50,
            column: 12,
            name_end_line: None,
            name_end_column: None,
            range_start_line: Some(50),
            range_start_column: Some(5),
            end_line: Some(50),
            end_column: Some(30),
            degraded_column: None,
        };

        let original = LspCallItem {
            name: "process_request".to_string(),
            kind: SymbolKind::Function,
            location: loc,
            call_site: Some(call_site),
        };

        let wire = CallItem::from(&original);
        let back: LspCallItem = wire.into();

        assert_eq!(back.name, original.name);
        assert_eq!(back.kind, original.kind);
        assert_eq!(back.location.file, original.location.file);
        assert_eq!(back.location.line, original.location.line);
        assert_eq!(back.location.column, original.location.column);
        // CallItem wire format stores file/line/column + degraded_column for the
        // main location (range fields are lost — it uses Location::point); the
        // degradation flag round-trips so daemon == in-process disclosure.
        assert_eq!(back.location.degraded_column, Some(true));
        // call_site preserves all fields through wire::Location.
        assert!(back.call_site.is_some());
        let back_site = back.call_site.unwrap();
        assert_eq!(back_site.file, PathBuf::from("src/handler.rs"));
        assert_eq!(back_site.line, 50);
        assert_eq!(back_site.column, 12);
        assert_eq!(back_site.range_start_line, Some(50));
        assert_eq!(back_site.range_start_column, Some(5));
        assert_eq!(back_site.end_line, Some(50));
        assert_eq!(back_site.end_column, Some(30));
    }

    #[test]
    fn call_item_roundtrip_without_call_site() {
        let loc = SymLocation::point(PathBuf::from("lib.rs"), 7, 3);

        let original = LspCallItem {
            name: "helper".to_string(),
            kind: SymbolKind::Method,
            location: loc,
            call_site: None,
        };

        let wire = CallItem::from(&original);
        let back: LspCallItem = wire.into();

        assert_eq!(back.name, "helper");
        assert_eq!(back.kind, SymbolKind::Method);
        assert_eq!(back.location.file, PathBuf::from("lib.rs"));
        assert_eq!(back.location.line, 7);
        assert_eq!(back.location.column, 3);
        assert!(back.call_site.is_none());
    }

    #[test]
    fn call_item_kind_roundtrip() {
        let loc = SymLocation::point(PathBuf::from("test.rs"), 1, 1);
        let original = LspCallItem {
            name: "new".to_string(),
            kind: SymbolKind::Constructor,
            location: loc,
            call_site: None,
        };

        let wire = CallItem::from(&original);
        assert_eq!(wire.kind, "constructor");

        let back: LspCallItem = wire.into();
        assert_eq!(back.kind, SymbolKind::Constructor);
    }

    // ---------------------------------------------------------------
    // TypeHierarchyItem roundtrip tests
    // ---------------------------------------------------------------

    #[test]
    fn type_hierarchy_item_roundtrip_with_detail() {
        // A degraded column on a supertypes/subtypes result must survive the
        // wire so daemon and in-process disclose it identically.
        let loc =
            SymLocation::point(PathBuf::from("src/models.rs"), 15, 4).with_degraded_column(true);

        let original = LspTypeItem {
            name: "Animal".to_string(),
            kind: SymbolKind::Interface,
            location: loc,
            detail: Some("crate::models".to_string()),
        };

        let wire = TypeHierarchyItem::from(&original);
        let back: LspTypeItem = wire.into();

        assert_eq!(back.name, "Animal");
        assert_eq!(back.kind, SymbolKind::Interface);
        assert_eq!(back.location.file, PathBuf::from("src/models.rs"));
        assert_eq!(back.location.line, 15);
        assert_eq!(back.location.column, 4);
        assert_eq!(back.location.degraded_column, Some(true));
        assert_eq!(back.detail, Some("crate::models".to_string()));
    }

    #[test]
    fn type_hierarchy_item_roundtrip_without_detail() {
        let loc = SymLocation::point(PathBuf::from("src/types.rs"), 8, 1);

        let original = LspTypeItem {
            name: "Dog".to_string(),
            kind: SymbolKind::Class,
            location: loc,
            detail: None,
        };

        let wire = TypeHierarchyItem::from(&original);
        let back: LspTypeItem = wire.into();

        assert_eq!(back.name, "Dog");
        assert_eq!(back.kind, SymbolKind::Class);
        assert_eq!(back.location.file, PathBuf::from("src/types.rs"));
        assert_eq!(back.location.line, 8);
        assert_eq!(back.location.column, 1);
        assert_eq!(back.detail, None);
    }

    #[test]
    fn type_hierarchy_item_kind_roundtrip() {
        let loc = SymLocation::point(PathBuf::from("test.rs"), 1, 1);
        let original = LspTypeItem {
            name: "MyStruct".to_string(),
            kind: SymbolKind::Struct,
            location: loc,
            detail: None,
        };

        let wire = TypeHierarchyItem::from(&original);
        assert_eq!(wire.kind, "struct");

        let back: LspTypeItem = wire.into();
        assert_eq!(back.kind, SymbolKind::Struct);
    }

    #[test]
    fn related_information_degraded_column_survives_the_wire() {
        // A diagnostic's related-info points at a cross-file location that can
        // degrade; the flag must survive serialization so daemon and in-process
        // diagnostics disclose it identically (invariant 3).
        let ri = RelatedInformation {
            file: "src/other.rs".to_string(),
            line: 3,
            column: 7,
            degraded_column: Some(true),
            message: "originally defined here".to_string(),
        };
        let json = serde_json::to_string(&ri).unwrap();
        let back: RelatedInformation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.degraded_column, Some(true));

        // Omitted when absent — no filler key in the common (non-degraded) case.
        let clean = RelatedInformation {
            degraded_column: None,
            ..ri
        };
        assert!(
            !serde_json::to_string(&clean)
                .unwrap()
                .contains("degraded_column")
        );
    }
}
