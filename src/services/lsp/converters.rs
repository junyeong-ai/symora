//! Type conversion functions for LSP responses

use std::path::Path;

use crate::infra::lsp::protocol::{
    DocumentSymbol, HoverContents, LspLocation, LspSymbolKind, Range,
};
use crate::models::lsp::{
    FileChangeWithEdits, FindSymbolsOptions, ParameterInfo, SignatureHelp, SignatureInfo,
    uri_to_path,
};
use crate::models::symbol::{Location, Symbol, SymbolKind};

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

pub(super) fn convert_location(loc: &LspLocation) -> Location {
    let is_default_range = loc.range.start.line == 0
        && loc.range.start.character == 0
        && loc.range.end.line == 0
        && loc.range.end.character == 0;

    if is_default_range {
        tracing::debug!("LSP returned location without range data: {}", loc.uri);
    }

    Location {
        file: uri_to_path(&loc.uri),
        line: loc.range.start.line + 1,
        column: loc.range.start.character + 1,
        end_line: Some(loc.range.end.line + 1),
        end_column: Some(loc.range.end.character + 1),
    }
}

pub(super) fn range_to_location(file: &Path, range: &Range) -> Location {
    Location {
        file: file.to_path_buf(),
        line: range.start.line + 1,
        column: range.start.character + 1,
        end_line: Some(range.end.line + 1),
        end_column: Some(range.end.character + 1),
    }
}

pub(super) fn uri_range_to_location(uri: &str, range: &Range) -> Location {
    Location {
        file: uri_to_path(uri),
        line: range.start.line + 1,
        column: range.start.character + 1,
        end_line: Some(range.end.line + 1),
        end_column: Some(range.end.character + 1),
    }
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
            let location = Location::new(
                file.to_path_buf(),
                doc_sym.selection_range.start.line + 1,
                doc_sym.selection_range.start.character + 1,
                doc_sym.range.end.line + 1,
                doc_sym.range.end.character + 1,
            );

            let mut symbol = Symbol::new(
                doc_sym.name.clone(),
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

pub(super) fn extract_body(content: &str, location: &Location) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let start_line = location.line.saturating_sub(1) as usize;
    let end_line = location.end_line.unwrap_or(location.line).saturating_sub(1) as usize;

    if start_line >= lines.len() {
        return None;
    }

    let end_line = end_line.min(lines.len() - 1);
    Some(lines[start_line..=end_line].join("\n"))
}

pub(super) fn extract_body_from_range(content: &str, range: &Range) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let start_line = range.start.line as usize;
    let end_line = range.end.line as usize;

    if start_line >= lines.len() {
        return None;
    }

    let end_line = end_line.min(lines.len() - 1);
    Some(lines[start_line..=end_line].join("\n"))
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

pub(super) fn parse_text_edits(edits: &serde_json::Value) -> Vec<crate::models::lsp::TextEdit> {
    use crate::models::lsp::{Position as LspPos, Range as LspRange, TextEdit as LspTextEdit};

    let arr = match edits.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    arr.iter()
        .filter_map(|edit| {
            let range = edit.get("range")?;
            let start = range.get("start")?;
            let end = range.get("end")?;

            Some(LspTextEdit {
                range: LspRange {
                    start: LspPos {
                        line: start.get("line")?.as_u64()? as u32,
                        character: start.get("character")?.as_u64()? as u32,
                    },
                    end: LspPos {
                        line: end.get("line")?.as_u64()? as u32,
                        character: end.get("character")?.as_u64()? as u32,
                    },
                },
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
                                    label.get(start..end).map(|s| s.to_string())
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
