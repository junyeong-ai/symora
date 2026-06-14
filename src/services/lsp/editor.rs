use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspFeature;
use crate::models::lsp::{
    ApplyActionResult, CodeAction, CodeActionKind, CodeLens, CodeLensCommand, FoldingRange,
    FoldingRangeKind, InlayHint, InlayHintKind, SelectionRange, TextEdit, path_to_uri,
};

use super::converters::*;
use super::helpers::*;
use super::position::PositionConverter;
use super::service::{DefaultLspService, ensure_indexed};

/// The largest valid LSP position component: positions are `uinteger`
/// (0 ..= 2^31-1). Used as the whole-line / whole-file end sentinel — the server
/// clamps it to the real line and document end — instead of `u32::MAX`, which
/// exceeds the LSP range.
const LSP_POSITION_MAX: u32 = i32::MAX as u32;

pub(super) async fn inlay_hints(
    service: &DefaultLspService,
    file: &Path,
    start_line: u32,
    end_line: u32,
) -> Result<Vec<InlayHint>, LspError> {
    check_feature_support(file, LspFeature::InlayHints)?;

    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                // The surface is line-granular, so the wire range spans whole
                // lines: column 0 through the max-LSP-uinteger end sentinel, which
                // the server clamps to the real line and document end. The end
                // bounds are capped to `LSP_POSITION_MAX` because LSP positions
                // are `uinteger` (0 ..= 2^31-1); `u32::MAX` exceeds that and a
                // strict server may reject it. Both bounds are encoding-invariant,
                // so unlike the response positions (decoded at the boundary
                // below) the request range needs no scalar→wire conversion.
                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start_line.min(LSP_POSITION_MAX), "character": 0 },
                        "end": {
                            "line": end_line.min(LSP_POSITION_MAX),
                            "character": LSP_POSITION_MAX
                        }
                    }
                });

                let encoding = client.position_encoding().await;
                let mut conv = PositionConverter::new(encoding).with_content(&file, &content);

                let hints: Option<Vec<serde_json::Value>> = client
                    .request("textDocument/inlayHint", Some(params))
                    .await?;

                Ok(hints
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|h| {
                        let mut pos = parse_position(h.get("position")?)?;
                        // Decode the wire column to a 0-indexed Unicode scalar at
                        // the boundary; the CLI applies +1 for display.
                        pos.character = conv.scalar_offset(&file, pos.line, pos.character);

                        let label = match h.get("label")? {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Array(arr) => arr
                                .iter()
                                .filter_map(|part| part.get("value").and_then(|v| v.as_str()))
                                .collect::<Vec<_>>()
                                .join(""),
                            _ => return None,
                        };

                        let kind = InlayHintKind::from_lsp(
                            h.get("kind").and_then(|k| k.as_u64()).map(|k| k as u32),
                        );
                        let padding_left = h
                            .get("paddingLeft")
                            .and_then(|p| p.as_bool())
                            .unwrap_or(false);
                        let padding_right = h
                            .get("paddingRight")
                            .and_then(|p| p.as_bool())
                            .unwrap_or(false);

                        Some(InlayHint {
                            position: pos,
                            label,
                            kind,
                            padding_left,
                            padding_right,
                        })
                    })
                    .collect())
            }
        })
        .await
}

pub(super) async fn folding_ranges(
    service: &DefaultLspService,
    file: &Path,
) -> Result<Vec<FoldingRange>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = serde_json::json!({
                    "textDocument": { "uri": uri }
                });

                let encoding = client.position_encoding().await;
                let mut conv = PositionConverter::new(encoding).with_content(&file, &content);

                let ranges: Option<Vec<serde_json::Value>> = client
                    .request("textDocument/foldingRange", Some(params))
                    .await?;

                Ok(ranges
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|r| {
                        let start_line = r.get("startLine")?.as_u64()? as u32;
                        let end_line = r.get("endLine")?.as_u64()? as u32;
                        // Decode wire columns to 0-indexed Unicode scalar at the
                        // boundary; the CLI applies +1 for display.
                        let start_character = r
                            .get("startCharacter")
                            .and_then(|v| v.as_u64())
                            .map(|v| conv.scalar_offset(&file, start_line, v as u32));
                        let end_character = r
                            .get("endCharacter")
                            .and_then(|v| v.as_u64())
                            .map(|v| conv.scalar_offset(&file, end_line, v as u32));
                        let kind =
                            FoldingRangeKind::from_lsp(r.get("kind").and_then(|k| k.as_str()));
                        let collapsed_text = r
                            .get("collapsedText")
                            .and_then(|t| t.as_str())
                            .map(String::from);

                        Some(FoldingRange {
                            start_line,
                            end_line,
                            start_character,
                            end_character,
                            kind,
                            collapsed_text,
                        })
                    })
                    .collect())
            }
        })
        .await
}

pub(super) async fn selection_ranges(
    service: &DefaultLspService,
    file: &Path,
    positions: Vec<(u32, u32)>,
) -> Result<Vec<SelectionRange>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let positions = positions.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let encoding = client.position_encoding().await;
                let lsp_positions: Vec<_> = positions
                    .iter()
                    .map(|(line, col)| to_lsp_position(*line, *col, &content, encoding))
                    .collect();

                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "positions": lsp_positions
                });

                let ranges: Option<Vec<serde_json::Value>> = client
                    .request("textDocument/selectionRange", Some(params))
                    .await?;

                let mut conv = PositionConverter::new(encoding).with_content(&file, &content);

                fn parse_selection_range(
                    value: &serde_json::Value,
                    conv: &mut PositionConverter,
                    file: &Path,
                ) -> Option<SelectionRange> {
                    let mut range = parse_range(value.get("range")?)?;
                    // Decode wire columns to 0-indexed Unicode scalar at the
                    // boundary, recursively through parent ranges; the CLI
                    // applies +1 for display.
                    range.start.character =
                        conv.scalar_offset(file, range.start.line, range.start.character);
                    range.end.character =
                        conv.scalar_offset(file, range.end.line, range.end.character);

                    let parent = value
                        .get("parent")
                        .and_then(|p| parse_selection_range(p, conv, file).map(Box::new));

                    Some(SelectionRange { range, parent })
                }

                Ok(ranges
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|v| parse_selection_range(v, &mut conv, &file))
                    .collect())
            }
        })
        .await
}

pub(super) async fn code_lenses(
    service: &DefaultLspService,
    file: &Path,
) -> Result<Vec<CodeLens>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = serde_json::json!({
                    "textDocument": { "uri": uri }
                });

                let lenses: Option<Vec<serde_json::Value>> = client
                    .request("textDocument/codeLens", Some(params))
                    .await?;

                let encoding = client.position_encoding().await;
                let mut conv = PositionConverter::new(encoding).with_content(&file, &content);

                Ok(lenses
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|lens| {
                        let mut range = parse_range(lens.get("range")?)?;
                        // Decode wire columns to 0-indexed Unicode scalar at the
                        // boundary; the CLI applies +1 for display.
                        range.start.character =
                            conv.scalar_offset(&file, range.start.line, range.start.character);
                        range.end.character =
                            conv.scalar_offset(&file, range.end.line, range.end.character);

                        let command = lens.get("command").and_then(|cmd| {
                            Some(CodeLensCommand {
                                title: cmd.get("title")?.as_str()?.to_string(),
                                command: cmd.get("command")?.as_str()?.to_string(),
                                arguments: cmd
                                    .get("arguments")
                                    .and_then(|a| a.as_array())
                                    .cloned()
                                    .unwrap_or_default(),
                            })
                        });

                        let data = lens.get("data").cloned();

                        Some(CodeLens {
                            range,
                            command,
                            data,
                        })
                    })
                    .collect())
            }
        })
        .await
}

pub(super) async fn code_actions(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Vec<CodeAction>, LspError> {
    // No static capability gate: code actions are near-universal and
    // get_support_level never yields None for CodeActions, so a gate here would
    // be inert dead code. A server that lacks them returns -32601 (mapped to
    // honest Unsupported) or an empty set (an honest "no actions here").
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                ensure_indexed(&client, &file, manager.root()).await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let encoding = client.position_encoding().await;
                // Both ends are derived from scalar columns: a 1-scalar-wide
                // range over the token at the cursor. Advancing the wire offset
                // by 1 instead would split a multibyte char or UTF-16 surrogate
                // pair on a non-ASCII line.
                let start = to_lsp_position(line, column, &content, encoding);
                let end = to_lsp_position(line, column + 1, &content, encoding);

                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start.line, "character": start.character },
                        "end": { "line": end.line, "character": end.character }
                    },
                    "context": { "diagnostics": [] }
                });

                let response: Option<Vec<serde_json::Value>> = client
                    .request("textDocument/codeAction", Some(params))
                    .await?;

                let actions = response
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| {
                        let title = v.get("title")?.as_str()?.to_string();
                        let kind = v.get("kind").and_then(|k| k.as_str());
                        let is_preferred = v
                            .get("isPreferred")
                            .and_then(|p| p.as_bool())
                            .unwrap_or(false);
                        let diagnostics = v
                            .get("diagnostics")
                            .and_then(|d| d.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
                                    .map(|s| s.to_string())
                                    .collect()
                            })
                            .unwrap_or_default();

                        Some(CodeAction {
                            title,
                            kind: CodeActionKind::from(kind),
                            is_preferred,
                            diagnostics,
                            edit: None,
                            data: Some(v),
                        })
                    })
                    .collect();

                Ok(actions)
            }
        })
        .await
}

pub(super) async fn apply_code_action(
    service: &DefaultLspService,
    file: &Path,
    action: &CodeAction,
) -> Result<ApplyActionResult, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let raw_action = match &action.data {
        Some(data) => data.clone(),
        None => {
            tracing::warn!("Code action has no data, cannot apply");
            return Ok(ApplyActionResult { changes: vec![] });
        }
    };

    let file = file.to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let raw_action = raw_action.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let mut edit = raw_action.get("edit").cloned();

                if edit.is_none() {
                    tracing::debug!("Code action has no edit, attempting resolve");

                    // A resolve failure propagates: swallowing it would
                    // report "applied, zero changes" for an action that
                    // was never materialized. Erring also routes the
                    // retryable codes through `execute_with_retry`.
                    let resolved: Option<serde_json::Value> = client
                        .request("codeAction/resolve", Some(raw_action.clone()))
                        .await?;
                    edit = resolved.as_ref().and_then(|r| r.get("edit").cloned());
                }

                // No edit anywhere means the action is command-only (the
                // server executes it itself) — symora doesn't run server
                // commands, and an empty success would misreport that.
                let Some(edit) = edit else {
                    return Err(LspError::Protocol(
                        "Code action provided no workspace edit (command-only \
                         actions are not supported). Pick a different action \
                         from `actions list`."
                            .to_string(),
                    ));
                };

                if let Some(kind) = find_resource_operation(&edit) {
                    return Err(LspError::Protocol(format!(
                        "Code action requires a file {kind} operation, which \
                         symora does not apply. Pick a different action from \
                         `actions list`.",
                    )));
                }

                let changes = parse_workspace_edit(&edit, client.position_encoding().await)?;
                Ok(ApplyActionResult { changes })
            }
        })
        .await
}

pub(super) async fn format(
    service: &DefaultLspService,
    file: &Path,
) -> Result<Vec<TextEdit>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "options": {
                        "tabSize": 4,
                        "insertSpaces": true,
                        "trimTrailingWhitespace": true,
                        "insertFinalNewline": true,
                        "trimFinalNewlines": true,
                    }
                });

                let result: serde_json::Value = client
                    .request("textDocument/formatting", Some(params))
                    .await?;

                if result.is_null() {
                    return Ok(Vec::new());
                }

                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);
                let edits = parse_text_edits(&result, &file, &mut conv)?;
                Ok(edits)
            }
        })
        .await
}
