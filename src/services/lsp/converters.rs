use std::path::Path;

use crate::infra::lsp::protocol::{
    DocumentSymbol, HoverContents, LspLocation, LspSymbolKind, Range,
};
use crate::models::lsp::{
    FileChangeWithEdits, FindSymbolsOptions, ParameterInfo, SignatureHelp, SignatureInfo,
    uri_to_path,
};
use crate::models::symbol::{Location, Symbol, SymbolKind};

use super::helpers::char_to_byte_index;

pub(super) fn convert_symbol_kind(kind: LspSymbolKind) -> SymbolKind {
    use LspSymbolKind as LspKind;
    match kind {
        LspKind::File => SymbolKind::File,
        LspKind::Module => SymbolKind::Module,
        LspKind::Namespace => SymbolKind::Namespace,
        LspKind::Package => SymbolKind::Package,
        LspKind::Class => SymbolKind::Class,
        LspKind::Method => SymbolKind::Method,
        LspKind::Property => SymbolKind::Property,
        LspKind::Field => SymbolKind::Field,
        LspKind::Constructor => SymbolKind::Constructor,
        LspKind::Enum => SymbolKind::Enum,
        LspKind::Interface => SymbolKind::Interface,
        LspKind::Function => SymbolKind::Function,
        LspKind::Variable => SymbolKind::Variable,
        LspKind::Constant => SymbolKind::Constant,
        LspKind::String => SymbolKind::String,
        LspKind::Number => SymbolKind::Number,
        LspKind::Boolean => SymbolKind::Boolean,
        LspKind::Array => SymbolKind::Array,
        LspKind::Object => SymbolKind::Object,
        LspKind::Key => SymbolKind::Key,
        LspKind::Null => SymbolKind::Null,
        LspKind::EnumMember => SymbolKind::EnumMember,
        LspKind::Struct => SymbolKind::Struct,
        LspKind::Event => SymbolKind::Event,
        LspKind::Operator => SymbolKind::Operator,
        LspKind::TypeParameter => SymbolKind::TypeParameter,
    }
}

fn location_from_range(file: std::path::PathBuf, range: &Range) -> Location {
    Location {
        file,
        line: range.start.line + 1,
        column: range.start.character + 1,
        range_start_line: None,
        range_start_column: None,
        end_line: Some(range.end.line + 1),
        end_column: Some(range.end.character + 1),
    }
}

pub(super) fn convert_location(loc: &LspLocation) -> Location {
    location_from_range(uri_to_path(&loc.uri), &loc.range)
}

pub(super) fn range_to_location(file: &Path, range: &Range) -> Location {
    location_from_range(file.to_path_buf(), range)
}

pub(super) fn uri_range_to_location(uri: &str, range: &Range) -> Location {
    location_from_range(uri_to_path(uri), range)
}

pub(super) fn extract_hover_content(contents: &HoverContents) -> String {
    match contents {
        HoverContents::String(s) => s.clone(),
        HoverContents::MarkupContent(mc) => mc.value.clone(),
        HoverContents::Array(arr) => arr
            .iter()
            .map(|ms| match ms {
                crate::infra::lsp::protocol::MarkedString::String(s) => s.clone(),
                crate::infra::lsp::protocol::MarkedString::LanguageString { value, .. } => {
                    value.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

pub(super) fn convert_document_symbols(
    symbols: &[DocumentSymbol],
    file: &Path,
    options: &FindSymbolsOptions,
    content: Option<&str>,
    container: Option<&str>,
    current_depth: u32,
) -> Vec<Symbol> {
    symbols
        .iter()
        .map(|doc_sym| {
            let location = Location::full(
                file.to_path_buf(),
                doc_sym.selection_range.start.line + 1,
                doc_sym.selection_range.start.character + 1,
                doc_sym.range.start.line + 1,
                doc_sym.range.start.character + 1,
                doc_sym.range.end.line + 1,
                doc_sym.range.end.character + 1,
            );

            let mut symbol = Symbol::new(
                Symbol::strip_type_parameters(&doc_sym.name),
                convert_symbol_kind(doc_sym.kind),
                location,
            );

            if let Some(c) = container {
                symbol = symbol.with_container(c);
            }

            if options.include_body
                && let Some(body) = content.and_then(|c| extract_body_from_range(c, &doc_sym.range))
            {
                symbol = symbol.with_body(body);
            }

            if current_depth < options.depth
                && let Some(children) = &doc_sym.children
            {
                let child_symbols = convert_document_symbols(
                    children,
                    file,
                    options,
                    content,
                    Some(&doc_sym.name),
                    current_depth + 1,
                );
                symbol = symbol.with_children(child_symbols);
            }

            symbol
        })
        .collect()
}

fn extract_lines(content: &str, start: usize, end: usize) -> Option<String> {
    let selected: Vec<&str> = content.lines().skip(start).take(end - start + 1).collect();
    if selected.is_empty() {
        return None;
    }
    Some(selected.join("\n"))
}

pub(super) fn apply_body_recursive(symbols: &mut [Symbol], content: &str) {
    for sym in symbols {
        if let Some(body) = extract_body(content, &sym.location) {
            sym.body = Some(body);
        }
        if !sym.children.is_empty() {
            apply_body_recursive(&mut sym.children, content);
        }
    }
}

pub(super) fn extract_body(content: &str, location: &Location) -> Option<String> {
    let start = location.line.saturating_sub(1) as usize;
    let end = location.end_line.unwrap_or(location.line).saturating_sub(1) as usize;
    extract_lines(content, start, end)
}

pub(super) fn extract_body_from_range(content: &str, range: &Range) -> Option<String> {
    extract_lines(content, range.start.line as usize, range.end.line as usize)
}

pub(super) fn parse_position(value: &serde_json::Value) -> Option<crate::models::lsp::Position> {
    let line = value.get("line")?.as_u64()? as u32;
    let character = value.get("character")?.as_u64()? as u32;
    Some(crate::models::lsp::Position::new(line, character))
}

pub(super) fn parse_range(value: &serde_json::Value) -> Option<crate::models::lsp::Range> {
    let start = parse_position(value.get("start")?)?;
    let end = parse_position(value.get("end")?)?;
    Some(crate::models::lsp::Range::new(start, end))
}

pub(super) fn parse_workspace_edit(edit: &serde_json::Value) -> Vec<FileChangeWithEdits> {
    let mut changes = Vec::new();

    if let Some(file_changes) = edit.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in file_changes {
            let file = uri_to_path(uri);
            let text_edits = parse_text_edits(edits);
            if !text_edits.is_empty() {
                changes.push(FileChangeWithEdits {
                    file,
                    edits: text_edits,
                });
            }
        }
    }

    if let Some(doc_changes) = edit.get("documentChanges").and_then(|c| c.as_array()) {
        for change in doc_changes {
            if let Some(text_doc) = change.get("textDocument") {
                let uri = text_doc.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                let file = uri_to_path(uri);

                if let Some(edits) = change.get("edits") {
                    let text_edits = parse_text_edits(edits);
                    if !text_edits.is_empty() {
                        changes.push(FileChangeWithEdits {
                            file,
                            edits: text_edits,
                        });
                    }
                }
            }
        }
    }

    changes
}

/// The first file create/rename/delete resource operation in a
/// workspace edit, if any. Symora applies text edits only — a caller
/// that ignored these would apply half an edit (references rewritten,
/// file never renamed) and still report success, so they must refuse.
pub(super) fn find_resource_operation(edit: &serde_json::Value) -> Option<&str> {
    edit.get("documentChanges")
        .and_then(|c| c.as_array())?
        .iter()
        .find_map(|change| match change.get("kind").and_then(|k| k.as_str()) {
            Some(kind @ ("create" | "rename" | "delete")) => Some(kind),
            _ => None,
        })
}

pub(super) fn parse_text_edits(edits: &serde_json::Value) -> Vec<crate::models::lsp::TextEdit> {
    use crate::models::lsp::TextEdit as LspTextEdit;

    let arr = match edits.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    arr.iter()
        .filter_map(|edit| {
            let range = parse_range(edit.get("range")?)?;

            Some(LspTextEdit {
                range,
                new_text: edit
                    .get("newText")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

pub(super) fn parse_signature_help(value: &serde_json::Value) -> Option<SignatureHelp> {
    let signatures = value.get("signatures")?.as_array()?;

    let parsed_signatures: Vec<SignatureInfo> = signatures
        .iter()
        .filter_map(|sig| {
            let label = sig.get("label")?.as_str()?.to_string();
            let documentation = sig.get("documentation").and_then(|d| {
                if let Some(s) = d.as_str() {
                    Some(s.to_string())
                } else {
                    d.get("value")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                }
            });

            let parameters: Vec<ParameterInfo> = sig
                .get("parameters")
                .and_then(|p| p.as_array())
                .map(|params| {
                    params
                        .iter()
                        .filter_map(|param| {
                            let param_label = param.get("label").and_then(|l| {
                                if let Some(s) = l.as_str() {
                                    Some(s.to_string())
                                } else if let Some(arr) = l.as_array() {
                                    let start = arr.first()?.as_u64()? as usize;
                                    let end = arr.get(1)?.as_u64()? as usize;
                                    let byte_start = char_to_byte_index(&label, start);
                                    let byte_end = char_to_byte_index(&label, end);
                                    label.get(byte_start..byte_end).map(|s| s.to_string())
                                } else {
                                    None
                                }
                            })?;

                            let param_doc = param.get("documentation").and_then(|d| {
                                if let Some(s) = d.as_str() {
                                    Some(s.to_string())
                                } else {
                                    d.get("value")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string())
                                }
                            });

                            Some(ParameterInfo {
                                label: param_label,
                                documentation: param_doc,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let active_parameter = sig
                .get("activeParameter")
                .and_then(|a| a.as_u64())
                .map(|a| a as u32);

            Some(SignatureInfo {
                label,
                documentation,
                parameters,
                active_parameter,
            })
        })
        .collect();

    if parsed_signatures.is_empty() {
        return None;
    }

    Some(SignatureHelp {
        signatures: parsed_signatures,
        active_signature: value
            .get("activeSignature")
            .and_then(|a| a.as_u64())
            .map(|a| a as u32),
        active_parameter: value
            .get("activeParameter")
            .and_then(|a| a.as_u64())
            .map(|a| a as u32),
    })
}

#[cfg(test)]
mod tests {
    use super::find_resource_operation;

    /// Text-only edits pass; any create/rename/delete resource operation
    /// is detected so callers refuse instead of applying half an edit.
    #[test]
    fn resource_operations_are_detected() {
        let text_only = serde_json::json!({
            "documentChanges": [
                { "textDocument": { "uri": "file:///a.rs" }, "edits": [] }
            ]
        });
        assert_eq!(find_resource_operation(&text_only), None);

        let with_rename = serde_json::json!({
            "documentChanges": [
                { "textDocument": { "uri": "file:///a.rs" }, "edits": [] },
                { "kind": "rename", "oldUri": "file:///a.rs", "newUri": "file:///b.rs" }
            ]
        });
        assert_eq!(find_resource_operation(&with_rename), Some("rename"));

        let changes_only = serde_json::json!({ "changes": { "file:///a.rs": [] } });
        assert_eq!(find_resource_operation(&changes_only), None);
    }
}
