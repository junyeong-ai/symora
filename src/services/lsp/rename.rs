use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::models::lsp::{PrepareRenameResult, RenameResult, path_to_uri};

use super::converters::*;
use super::helpers::*;
use super::position::encoded_offset_to_byte;
use super::service::{DefaultLspService, ensure_indexed};
use crate::infra::lsp::protocol::PositionEncoding;

pub(super) async fn prepare_rename(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Option<PrepareRenameResult>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                // Renameability is decided from the cross-file reference set,
                // so the same readiness gate every other LSP query uses must
                // run first — otherwise a cold server answers "no references"
                // for a symbol it simply has not indexed yet.
                ensure_indexed(&client, &file, manager.root()).await;
                client.sleep_for_cross_file_settle().await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let encoding = client.position_encoding().await;
                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column, &content, encoding)
                });

                let result: Result<Option<serde_json::Value>, _> = client
                    .request("textDocument/prepareRename", Some(params))
                    .await;

                let value = match result {
                    Ok(Some(v)) if !v.is_null() => v,
                    Ok(_) => return Ok(None),
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

                // Format 1: { placeholder, range } — only the placeholder is
                // used (it pre-fills the rename); the affected range is consumed
                // nowhere, so it is not carried.
                if let Some(placeholder) = value.get("placeholder").and_then(|p| p.as_str()) {
                    return Ok(Some(PrepareRenameResult {
                        placeholder: placeholder.to_string(),
                    }));
                }

                // Format 2: a bare Range — extract the placeholder from the
                // source span (wire offsets index the line's bytes).
                if let (Some(start), Some(end)) = (value.get("start"), value.get("end")) {
                    let start_pos = extract_position(start);
                    let end_pos = extract_position(end);
                    if let (Some(start_pos), Some(end_pos)) = (start_pos, end_pos) {
                        let placeholder = read_line_streaming(&file, start_pos.line)
                            .await
                            .and_then(|line| {
                                let s =
                                    encoded_offset_to_byte(encoding, &line, start_pos.character);
                                let e = encoded_offset_to_byte(encoding, &line, end_pos.character);
                                (s < e && e <= line.len()).then(|| line[s..e].to_string())
                            });

                        if let Some(placeholder) = placeholder {
                            return Ok(Some(PrepareRenameResult { placeholder }));
                        }
                    }
                }

                // Format 3: { defaultBehavior: true } — no placeholder text.
                if value.get("defaultBehavior").and_then(|v| v.as_bool()) == Some(true) {
                    return Ok(Some(PrepareRenameResult {
                        placeholder: String::new(),
                    }));
                }

                Ok(None)
            }
        })
        .await
}

/// The workspace edit that renames the symbol at a position, or `None` when
/// the server declines: nothing renameable is there. That is the protocol's
/// own answer to a position, not a failure of the exchange.
pub(super) async fn rename(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
    new_name: &str,
) -> Result<Option<RenameResult>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let content = read_file_validated(file, max_file_size).await?;
    let uri = path_to_uri(file);
    let new_name = new_name.to_string();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    let (result, encoding): (serde_json::Value, PositionEncoding) = service
        .execute_with_retry(&file, |client| {
            let uri = uri.clone();
            let content = content.clone();
            let new_name = new_name.clone();
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                // Rename rewrites references across the workspace, so wait for
                // the index to settle before issuing it — the same gate the
                // navigation queries use.
                ensure_indexed(&client, &file, manager.root()).await;
                client.sleep_for_cross_file_settle().await;

                client.sync_document(&uri, &content).await?;
                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column, &content, client.position_encoding().await),
                    "newName": new_name
                });
                let result = client.request("textDocument/rename", Some(params)).await?;
                Ok((result, client.position_encoding().await))
            }
        })
        .await?;

    if result.is_null() {
        return Ok(None);
    }

    if let Some(kind) = find_resource_operation(&result) {
        return Err(LspError::UnsupportedEdit(format!(
            "Rename requires a file {kind} operation, which symora does not \
             apply. Perform the file operation manually, then rename the \
             remaining references.",
        )));
    }

    let changes = parse_workspace_edit(&result, encoding)?;
    Ok(Some(RenameResult { changes }))
}
