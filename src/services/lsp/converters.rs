use std::path::Path;

use crate::error::LspError;
use crate::infra::lsp::protocol::{
    DocumentSymbol, HoverContents, LspLocation, LspSymbolKind, Range,
};
use crate::models::lsp::{
    FileChangeWithEdits, FindSymbolsOptions, ParameterInfo, SignatureHelp, SignatureInfo,
    uri_to_path,
};
use crate::models::symbol::{Location, Symbol, SymbolKind};

use super::position::{PositionConverter, encoded_offset_to_byte};
use crate::infra::lsp::protocol::PositionEncoding;

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

fn location_from_range(
    file: std::path::PathBuf,
    range: &Range,
    conv: &mut PositionConverter,
) -> Location {
    let (column, degraded) =
        conv.scalar_column_disclosed(&file, range.start.line, range.start.character);
    // The end column is decoded as the range bound, but its degradation is NOT
    // folded into the flag: `degraded_column` discloses whether the EMITTED
    // `column` (the start position) is a wire-offset guess. A stale or
    // EOF-exclusive range whose end line is out of range must not mark an
    // otherwise-cleanly-decoded start column as degraded. (When the file itself
    // is unreadable both ends degrade together, so the start alone still
    // discloses that case.)
    let (end_column, _) = conv.scalar_column_disclosed(&file, range.end.line, range.end.character);
    Location {
        file,
        line: range.start.line + 1,
        column,
        range_start_line: None,
        range_start_column: None,
        end_line: Some(range.end.line + 1),
        end_column: Some(end_column),
        degraded_column: degraded.then_some(true),
    }
}

pub(super) fn convert_location(loc: &LspLocation, conv: &mut PositionConverter) -> Location {
    location_from_range(uri_to_path(&loc.uri), &loc.range, conv)
}

pub(super) fn range_to_location(
    file: &Path,
    range: &Range,
    conv: &mut PositionConverter,
) -> Location {
    location_from_range(file.to_path_buf(), range, conv)
}

pub(super) fn uri_range_to_location(
    uri: &str,
    range: &Range,
    conv: &mut PositionConverter,
) -> Location {
    location_from_range(uri_to_path(uri), range, conv)
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
    conv: &mut PositionConverter,
) -> Vec<Symbol> {
    symbols
        .iter()
        .map(|doc_sym| {
            let sel = &doc_sym.selection_range;
            let range = &doc_sym.range;
            let (name_col, name_degraded) =
                conv.scalar_column_disclosed(file, sel.start.line, sel.start.character);
            // Only the name/selection position backs the emitted `column`, so
            // only its degradation sets the flag. The declaration-range endpoints
            // are secondary bounds — an out-of-range end (stale/EOF-exclusive
            // range) must not mark a cleanly-decoded name column as a guess.
            let (start_col, _) =
                conv.scalar_column_disclosed(file, range.start.line, range.start.character);
            let (end_col, _) =
                conv.scalar_column_disclosed(file, range.end.line, range.end.character);
            let location = Location::full(
                file.to_path_buf(),
                sel.start.line + 1,
                name_col,
                range.start.line + 1,
                start_col,
                range.end.line + 1,
                end_col,
            )
            .with_degraded_column(name_degraded);

            let display_name = Symbol::normalize_symbol_name(&doc_sym.name);
            let mut symbol = Symbol::new(
                display_name.clone(),
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
                    Some(&display_name),
                    current_depth + 1,
                    conv,
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

/// LSP `Diagnostic.code` is a string-or-number union. Surface the value
/// itself — a string's content, a number's decimal form — never the JSON
/// literal a blind re-serialization would produce (`"\"E0308\""`).
pub(super) fn diagnostic_code_string(code: serde_json::Value) -> String {
    match code {
        serde_json::Value::String(code) => code,
        other => other.to_string(),
    }
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

pub(super) fn parse_workspace_edit(
    edit: &serde_json::Value,
    encoding: PositionEncoding,
) -> Result<Vec<FileChangeWithEdits>, LspError> {
    // Each edit's range arrives in the negotiated wire encoding; it is decoded
    // to a native scalar column HERE (against the edit's own target file) so the
    // edit applier in edit.rs receives native coordinates and never has to know
    // an encoding exists. One converter caches every target file's lines.
    let mut conv = PositionConverter::new(encoding);
    // `documentChanges` and `changes` are two representations of the same
    // edit. A server that emits `documentChanges` (which Symora advertises
    // support for) must be read from there with `changes` ignored — reading
    // both would apply every edit twice. Fall back to `changes` only when
    // `documentChanges` is absent.
    // FAIL CLOSED on a malformed TOP-LEVEL shape too: a present-but-wrong-typed
    // `documentChanges`/`changes` is a malformed edit, never silently treated as
    // absent (which would drop edits or fall through to the other key).
    match edit.get("documentChanges") {
        Some(serde_json::Value::Array(doc_changes)) => {
            let mut changes = Vec::new();
            for change in doc_changes {
                // Resource ops (create/rename/delete) are refused by the caller
                // via find_resource_operation before we get here, so every
                // remaining documentChange must be a well-formed TextDocumentEdit.
                // FAIL CLOSED on a malformed one rather than silently dropping its
                // edit group while still applying the others.
                let Some(text_doc) = change.get("textDocument") else {
                    return Err(LspError::Protocol(
                        "malformed workspace edit: a documentChange has no textDocument"
                            .to_string(),
                    ));
                };
                let Some(uri) = text_doc
                    .get("uri")
                    .and_then(|u| u.as_str())
                    .filter(|u| !u.is_empty())
                else {
                    return Err(LspError::Protocol(
                        "malformed workspace edit: a documentChange textDocument has no uri"
                            .to_string(),
                    ));
                };
                let file = uri_to_path(uri);
                let Some(edits) = change.get("edits") else {
                    return Err(LspError::Protocol(format!(
                        "malformed workspace edit: documentChange for {} has no edits",
                        file.display(),
                    )));
                };
                let text_edits = parse_text_edits(edits, &file, &mut conv)?;
                if !text_edits.is_empty() {
                    changes.push(FileChangeWithEdits {
                        file,
                        edits: text_edits,
                    });
                }
            }
            return Ok(changes);
        }
        Some(_) => {
            return Err(LspError::Protocol(
                "malformed workspace edit: documentChanges must be an array".to_string(),
            ));
        }
        None => {}
    }

    let mut changes = Vec::new();
    match edit.get("changes") {
        Some(serde_json::Value::Object(file_changes)) => {
            for (uri, edits) in file_changes {
                if uri.is_empty() {
                    return Err(LspError::Protocol(
                        "malformed workspace edit: changes contains an empty uri".to_string(),
                    ));
                }
                let file = uri_to_path(uri);
                let text_edits = parse_text_edits(edits, &file, &mut conv)?;
                if !text_edits.is_empty() {
                    changes.push(FileChangeWithEdits {
                        file,
                        edits: text_edits,
                    });
                }
            }
        }
        Some(_) => {
            return Err(LspError::Protocol(
                "malformed workspace edit: changes must be an object".to_string(),
            ));
        }
        None => {}
    }
    Ok(changes)
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

pub(super) fn parse_text_edits(
    edits: &serde_json::Value,
    file: &Path,
    conv: &mut PositionConverter,
) -> Result<Vec<crate::models::lsp::TextEdit>, LspError> {
    use crate::models::lsp::TextEdit as LspTextEdit;

    let arr = match edits.as_array() {
        Some(a) => a,
        None => {
            return Err(LspError::Protocol(format!(
                "malformed text edits in {}: expected an array, refusing to apply",
                file.display(),
            )));
        }
    };

    let mut out = Vec::new();
    for edit in arr {
        // FAIL CLOSED on a malformed element: a missing/invalid range or a
        // missing newText is never silently skipped or coerced to an empty
        // deletion — half-applying a workspace edit corrupts the file.
        let Some(mut range) = edit.get("range").and_then(parse_range) else {
            return Err(LspError::Protocol(format!(
                "malformed text edit in {}: missing or invalid range, refusing to apply",
                file.display(),
            )));
        };
        let Some(new_text) = edit.get("newText").and_then(|t| t.as_str()) else {
            return Err(LspError::Protocol(format!(
                "malformed text edit in {}: missing newText, refusing to apply",
                file.display(),
            )));
        };
        // Decode the wire columns to native scalar offsets against the target
        // file so apply_text_edits slices the correct bytes. FAIL CLOSED if the
        // target line cannot be read: an edit must never be applied at a guessed
        // byte offset (silent corruption on a multibyte line).
        let (Some(start_char), Some(end_char)) = (
            conv.scalar_offset_checked(file, range.start.line, range.start.character),
            conv.scalar_offset_checked(file, range.end.line, range.end.character),
        ) else {
            return Err(LspError::Protocol(format!(
                "cannot decode edit range in {}: target line unreadable, refusing to \
                 apply at a guessed offset",
                file.display(),
            )));
        };
        range.start.character = start_char;
        range.end.character = end_char;
        out.push(LspTextEdit {
            range,
            new_text: new_text.to_string(),
        });
    }
    Ok(out)
}

pub(super) fn parse_signature_help(
    value: &serde_json::Value,
    encoding: PositionEncoding,
) -> Option<SignatureHelp> {
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
                                    // labelOffset indexes the label STRING in the
                                    // negotiated wire encoding, not a source line.
                                    let start = arr.first()?.as_u64()? as u32;
                                    let end = arr.get(1)?.as_u64()? as u32;
                                    let byte_start =
                                        encoded_offset_to_byte(encoding, &label, start);
                                    let byte_end = encoded_offset_to_byte(encoding, &label, end);
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
    use super::{
        PositionEncoding, diagnostic_code_string, find_resource_operation, parse_workspace_edit,
    };

    /// An edit range's wire column is decoded to a native scalar offset against
    /// the target file BEFORE it reaches the edit applier, so a non-BMP line
    /// cannot be sliced at the wrong byte. `let x = "😀";` — the closing quote is
    /// utf-16 unit 11 but scalar 10; a missing conversion would slice wrong.
    #[test]
    fn parse_text_edits_decodes_wire_columns_to_native_scalars() {
        use crate::services::lsp::position::PositionConverter;
        let file = std::path::Path::new("seeded.rs");
        let mut conv =
            PositionConverter::new(PositionEncoding::Utf16).with_content(file, "let x = \"😀\";");
        let edits = serde_json::json!([{
            "range": {
                "start": { "line": 0, "character": 11 },
                "end": { "line": 0, "character": 11 }
            },
            "newText": "Z"
        }]);
        let parsed = super::parse_text_edits(&edits, file, &mut conv).unwrap();
        assert_eq!(parsed.len(), 1);
        // utf-16 character 11 -> scalar 10 (the emoji counts as 1 scalar).
        assert_eq!(parsed[0].range.start.character, 10);
        assert_eq!(parsed[0].range.end.character, 10);
        // utf-8 server: the same logical position is byte offset 13 -> scalar 10.
        let mut conv8 =
            PositionConverter::new(PositionEncoding::Utf8).with_content(file, "let x = \"😀\";");
        let edits8 = serde_json::json!([{
            "range": {
                "start": { "line": 0, "character": 13 },
                "end": { "line": 0, "character": 13 }
            },
            "newText": "Z"
        }]);
        let parsed8 = super::parse_text_edits(&edits8, file, &mut conv8).unwrap();
        assert_eq!(parsed8[0].range.start.character, 10);
    }

    /// An edit whose target line cannot be read fails closed — refused, never
    /// applied at a guessed byte offset (the non-zero column is the risk; a
    /// column-0 position needs no line and still succeeds).
    #[test]
    fn unreadable_edit_line_fails_closed() {
        use crate::services::lsp::position::PositionConverter;
        use std::path::Path;
        let mut conv = PositionConverter::new(PositionEncoding::Utf16);
        let edits = serde_json::json!([{
            "range": { "start": { "line": 0, "character": 3 },
                       "end": { "line": 0, "character": 5 } },
            "newText": "x"
        }]);
        let r = super::parse_text_edits(&edits, Path::new("/nonexistent/zzz.rs"), &mut conv);
        assert!(
            r.is_err(),
            "a non-zero column on an unreadable line must fail closed"
        );
    }

    /// `degraded_column` tracks the EMITTED `column` (the range start), not the
    /// end bound: a clean start with an out-of-range end (a stale or
    /// EOF-exclusive range) must NOT falsely flag the column as a guess.
    #[test]
    fn location_from_range_flags_only_the_emitted_start_column() {
        use crate::infra::lsp::protocol::{Position, Range};
        use crate::services::lsp::position::PositionConverter;
        use std::path::Path;

        let file = Path::new("seeded.rs");
        let mut conv =
            PositionConverter::new(PositionEncoding::Utf16).with_content(file, "fn x() {}");
        // Start decodes on the readable first line; the end line is far past EOF.
        let range = Range {
            start: Position {
                line: 0,
                character: 3,
            },
            end: Position {
                line: 999,
                character: 0,
            },
        };
        let loc = super::location_from_range(file.to_path_buf(), &range, &mut conv);
        assert_eq!(
            loc.degraded_column, None,
            "a cleanly-decoded start must not be flagged because the end is out of range"
        );

        // A start on a genuinely unreadable line still degrades the column.
        let mut conv2 = PositionConverter::new(PositionEncoding::Utf16);
        let unreadable = Range {
            start: Position {
                line: 0,
                character: 3,
            },
            end: Position {
                line: 0,
                character: 5,
            },
        };
        let loc2 = super::location_from_range(
            Path::new("/nonexistent/zzz.rs").to_path_buf(),
            &unreadable,
            &mut conv2,
        );
        assert_eq!(loc2.degraded_column, Some(true));
    }

    #[test]
    fn malformed_edits_fail_closed() {
        use crate::services::lsp::position::PositionConverter;
        use std::path::Path;
        let file = Path::new("/x.rs");

        // A non-array edits payload is malformed — never silently empty.
        let mut conv = PositionConverter::new(PositionEncoding::Utf16);
        assert!(
            super::parse_text_edits(&serde_json::json!({"not": "an array"}), file, &mut conv)
                .is_err()
        );

        // A missing newText is malformed — never coerced to an empty deletion.
        let no_newtext = serde_json::json!([{
            "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0} }
        }]);
        assert!(super::parse_text_edits(&no_newtext, file, &mut conv).is_err());

        // A documentChange without edits is malformed — never silently dropped.
        let dc = serde_json::json!({
            "documentChanges": [ { "textDocument": { "uri": "file:///a.rs" } } ]
        });
        assert!(parse_workspace_edit(&dc, PositionEncoding::Utf16).is_err());

        // A missing range is malformed — never silently skipped.
        let no_range = serde_json::json!([{ "newText": "x" }]);
        assert!(super::parse_text_edits(&no_range, file, &mut conv).is_err());

        // An explicit empty newText is a legitimate deletion — it succeeds.
        let mut conv2 = PositionConverter::new(PositionEncoding::Utf16).with_content(file, "abc");
        let deletion = serde_json::json!([{
            "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0} },
            "newText": ""
        }]);
        let parsed = super::parse_text_edits(&deletion, file, &mut conv2).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].new_text, "");

        // A documentChange without textDocument, or with an empty uri, is
        // malformed — never silently skipped.
        let no_textdoc = serde_json::json!({ "documentChanges": [ { "edits": [] } ] });
        assert!(parse_workspace_edit(&no_textdoc, PositionEncoding::Utf16).is_err());
        let empty_uri = serde_json::json!({
            "documentChanges": [ { "textDocument": { "uri": "" }, "edits": [] } ]
        });
        assert!(parse_workspace_edit(&empty_uri, PositionEncoding::Utf16).is_err());

        // Top-level shape: documentChanges present but not an array is malformed.
        assert!(
            parse_workspace_edit(
                &serde_json::json!({"documentChanges": {}}),
                PositionEncoding::Utf16
            )
            .is_err()
        );
        // changes present but not an object is malformed.
        assert!(
            parse_workspace_edit(&serde_json::json!({"changes": []}), PositionEncoding::Utf16)
                .is_err()
        );
        // A changes map with an empty uri key is malformed.
        assert!(
            parse_workspace_edit(
                &serde_json::json!({"changes": {"": []}}),
                PositionEncoding::Utf16
            )
            .is_err()
        );
        // An edit with neither key is an empty no-op edit, not an error.
        assert!(
            parse_workspace_edit(&serde_json::json!({}), PositionEncoding::Utf16)
                .unwrap()
                .is_empty()
        );
    }

    /// Both arms of the LSP string-or-number union come out as the bare
    /// value — `"E0308"` stays `E0308`, `6133` becomes `6133`.
    #[test]
    fn diagnostic_code_unwraps_the_union() {
        assert_eq!(diagnostic_code_string(serde_json::json!("E0308")), "E0308");
        assert_eq!(diagnostic_code_string(serde_json::json!(6133)), "6133");
    }

    /// When a server fills both representations, `documentChanges` wins and
    /// `changes` is ignored — reading both would apply every edit twice.
    #[test]
    fn document_changes_take_precedence_over_changes() {
        let both = serde_json::json!({
            "changes": {
                "file:///a.rs": [
                    { "range": { "start": { "line": 0, "character": 0 },
                                 "end": { "line": 0, "character": 1 } },
                      "newText": "X" }
                ]
            },
            "documentChanges": [
                { "textDocument": { "uri": "file:///a.rs" },
                  "edits": [
                    { "range": { "start": { "line": 0, "character": 0 },
                                 "end": { "line": 0, "character": 0 } },
                      "newText": "X" }
                  ] }
            ]
        });
        let parsed = parse_workspace_edit(&both, PositionEncoding::Utf16).unwrap();
        assert_eq!(parsed.len(), 1, "the file appears once, not duplicated");
        assert_eq!(parsed[0].edits.len(), 1);
    }

    /// With no `documentChanges`, the legacy `changes` map is read.
    #[test]
    fn changes_map_is_read_when_document_changes_absent() {
        let changes_only = serde_json::json!({
            "changes": {
                "file:///a.rs": [
                    { "range": { "start": { "line": 0, "character": 0 },
                                 "end": { "line": 0, "character": 0 } },
                      "newText": "X" }
                ]
            }
        });
        assert_eq!(
            parse_workspace_edit(&changes_only, PositionEncoding::Utf16)
                .unwrap()
                .len(),
            1
        );
    }

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
