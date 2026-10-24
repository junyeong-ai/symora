use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::models::lsp::{ApplyActionResult, CodeAction, CodeActionKind, path_to_uri};

use super::converters::*;
use super::helpers::*;
use super::service::{DefaultLspService, ensure_indexed};

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

                let mut actions: Vec<CodeAction> = response
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
                            expected_hash: Some(crate::infra::hash_content(&content)),
                            document_hashes: Default::default(),
                            title,
                            kind: CodeActionKind::from(kind),
                            is_preferred,
                            diagnostics,
                            edit: None,
                            data: Some(v),
                        })
                    })
                    .collect();

                for action in &mut actions {
                    if let Some(edit) = action.data.as_ref().and_then(|raw| raw.get("edit")) {
                        for uri in workspace_edit_uris(edit) {
                            let path = crate::models::lsp::uri_to_path(uri);
                            let snapshot = read_file_validated(&path, max_file_size).await?;
                            action
                                .document_hashes
                                .insert(uri.to_string(), crate::infra::hash_content(&snapshot));
                        }
                    }
                }
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
    let expected_hash = action.expected_hash;
    let document_hashes = action.document_hashes.clone();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let raw_action = raw_action.clone();
            let document_hashes = document_hashes.clone();
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                verify_document_snapshot(&file, &content, expected_hash)?;
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
                    return Err(LspError::UnsupportedEdit(
                        "Code action provided no workspace edit (command-only \
                         actions are not supported). Pick a different action \
                         from `actions list`."
                            .to_string(),
                    ));
                };

                if let Some(kind) = find_resource_operation(&edit) {
                    return Err(LspError::UnsupportedEdit(format!(
                        "Code action requires a file {kind} operation, which \
                         symora does not apply. Pick a different action from \
                         `actions list`.",
                    )));
                }

                let snapshots = client.validate_workspace_edit(&edit).await?;
                verify_workspace_edit_target(&file, expected_hash, &snapshots)?;
                for (uri, hash) in &document_hashes {
                    let path = crate::models::lsp::uri_to_path(uri);
                    if let Some(content) = snapshots.get(&path) {
                        verify_document_snapshot(&path, content, Some(*hash))?;
                    }
                }
                let changes = parse_workspace_edit_snapshot(
                    &edit,
                    client.position_encoding().await,
                    &snapshots,
                )?;
                Ok(ApplyActionResult { changes })
            }
        })
        .await
}

fn workspace_edit_uris(edit: &serde_json::Value) -> Vec<&str> {
    if let Some(changes) = edit
        .get("documentChanges")
        .and_then(serde_json::Value::as_array)
    {
        return changes
            .iter()
            .filter_map(|change| {
                let edits = change.get("edits")?.as_array()?;
                (!edits.is_empty())
                    .then(|| change.get("textDocument")?.get("uri")?.as_str())
                    .flatten()
            })
            .collect();
    }
    edit.get("changes")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(|changes| {
            changes.iter().filter_map(|(uri, edits)| {
                edits
                    .as_array()
                    .is_some_and(|edits| !edits.is_empty())
                    .then_some(uri.as_str())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::lsp::protocol::error_codes;

    #[test]
    fn code_actions_reject_changed_or_missing_request_snapshots() {
        let file = Path::new("main.go");
        let expected = Some(crate::infra::hash_content("abc()\n"));
        assert!(verify_document_snapshot(file, "abc()\n", expected).is_ok());
        assert_eq!(
            verify_document_snapshot(file, "prefixabc()\n", expected)
                .unwrap_err()
                .error_code(),
            error_codes::CONTENT_MODIFIED
        );
        assert!(verify_document_snapshot(file, "abc()\n", None).is_err());
    }
}
