use std::path::Path;

use crate::error::LspError;
use crate::models::lsp::{PrepareRenameResult, RenameResult, path_to_uri};

use super::converters::*;
use super::helpers::*;
use super::service::DefaultLspService;

pub(super) async fn prepare_rename(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Option<PrepareRenameResult>, LspError> {
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
                    "position": to_lsp_position(line, column)
                });

                let result: Result<Option<serde_json::Value>, _> = client
                    .request("textDocument/prepareRename", Some(params))
                    .await;

                let value = match result {
                    Ok(Some(v)) if !v.is_null() => v,
                    Ok(_) => return Ok(None),
                    Err(LspError::Protocol(msg)) if msg.contains("cannot be renamed") => {
                        return Ok(None);
                    }
                    Err(e) => return Err(e),
                };

                fn extract_position(
                    pos: &serde_json::Value,
                ) -> Option<crate::models::lsp::Position> {
                    Some(crate::models::lsp::Position {
                        line: pos.get("line")?.as_u64()? as u32,
                        character: pos.get("character")?.as_u64()? as u32,
                    })
                }

                // Format 1: { placeholder: string, range: Range }
                if let Some(placeholder) = value.get("placeholder").and_then(|p| p.as_str())
                    && let Some(range) = value.get("range")
                {
                    let start = range.get("start").and_then(extract_position);
                    let end = range.get("end").and_then(extract_position);
                    if let (Some(start), Some(end)) = (start, end) {
                        return Ok(Some(PrepareRenameResult {
                            placeholder: placeholder.to_string(),
                            range: crate::models::lsp::Range { start, end },
                        }));
                    }
                }

                // Format 2: Range (just start/end positions, extract placeholder from source)
                if let (Some(start), Some(end)) = (value.get("start"), value.get("end")) {
                    let start_pos = extract_position(start);
                    let end_pos = extract_position(end);
                    if let (Some(start_pos), Some(end_pos)) = (start_pos, end_pos) {
                        let placeholder = read_line_streaming(&file, start_pos.line)
                            .await
                            .and_then(|line| {
                                let s = char_to_byte_index(&line, start_pos.character as usize);
                                let e = char_to_byte_index(&line, end_pos.character as usize);
                                if s < e && e <= line.len() {
                                    Some(line[s..e].to_string())
                                } else {
                                    None
                                }
                            });

                        if let Some(placeholder) = placeholder {
                            return Ok(Some(PrepareRenameResult {
                                placeholder,
                                range: crate::models::lsp::Range {
                                    start: start_pos,
                                    end: end_pos,
                                },
                            }));
                        }
                    }
                }

                // Format 3: { defaultBehavior: true }
                if value.get("defaultBehavior").and_then(|v| v.as_bool()) == Some(true) {
                    return Ok(Some(PrepareRenameResult {
                        placeholder: String::new(),
                        range: crate::models::lsp::Range {
                            start: crate::models::lsp::Position {
                                line: line.saturating_sub(1),
                                character: column.saturating_sub(1),
                            },
                            end: crate::models::lsp::Position {
                                line: line.saturating_sub(1),
                                character: column,
                            },
                        },
                    }));
                }

                Ok(None)
            }
        })
        .await
}

pub(super) async fn rename(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
    new_name: &str,
) -> Result<RenameResult, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let content = read_file_validated(file, max_file_size).await?;
    let uri = path_to_uri(file);
    let new_name = new_name.to_string();

    let result: serde_json::Value = service
        .execute_with_retry(file, |client| {
            let uri = uri.clone();
            let content = content.clone();
            let new_name = new_name.clone();
            async move {
                client.sync_document(&uri, &content).await?;
                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column),
                    "newName": new_name
                });
                client.request("textDocument/rename", Some(params)).await
            }
        })
        .await?;

    if result.is_null() {
        return Err(LspError::Protocol(
            "Symbol at this position cannot be renamed. Try a different position or symbol."
                .to_string(),
        ));
    }

    if let Some(kind) = find_resource_operation(&result) {
        return Err(LspError::Protocol(format!(
            "Rename requires a file {kind} operation, which symora does not \
             apply. Perform the file operation manually, then rename the \
             remaining references.",
        )));
    }

    let changes = parse_workspace_edit(&result);
    Ok(RenameResult { changes })
}
