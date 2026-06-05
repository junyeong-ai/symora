use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspFeature;
use crate::infra::lsp::protocol::{
    CallHierarchyIncomingCall, CallHierarchyOutgoingCall, LspCallHierarchyItem,
};
use crate::models::lsp::{CallHierarchyItem, TypeHierarchyItem, path_to_uri};

use super::converters::*;
use super::helpers::*;
use super::service::{DefaultLspService, ensure_indexed};

pub(super) async fn incoming_calls(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Vec<CallHierarchyItem>, LspError> {
    call_hierarchy(service, file, line, column, true).await
}

pub(super) async fn outgoing_calls(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Vec<CallHierarchyItem>, LspError> {
    call_hierarchy(service, file, line, column, false).await
}

async fn call_hierarchy(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
    incoming: bool,
) -> Result<Vec<CallHierarchyItem>, LspError> {
    let feature = if incoming {
        LspFeature::IncomingCalls
    } else {
        LspFeature::OutgoingCalls
    };
    check_feature_support(file, feature)?;

    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                ensure_indexed(&client, &file, manager.root()).await;
                client.sleep_for_cross_file_settle().await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let prepare_params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column)
                });

                let items: Option<Vec<LspCallHierarchyItem>> = client
                    .request("textDocument/prepareCallHierarchy", Some(prepare_params))
                    .await?;

                let items = items.unwrap_or_default();
                if items.is_empty() {
                    return Ok(vec![]);
                }

                let follow_params = serde_json::json!({ "item": items.first().unwrap() });

                if incoming {
                    let calls: Option<Vec<CallHierarchyIncomingCall>> = client
                        .request("callHierarchy/incomingCalls", Some(follow_params))
                        .await?;

                    Ok(calls
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| CallHierarchyItem {
                            name: c.from.name,
                            kind: convert_symbol_kind(c.from.kind),
                            location: uri_range_to_location(&c.from.uri, &c.from.selection_range),
                            call_site: c
                                .from_ranges
                                .first()
                                .map(|r| uri_range_to_location(&c.from.uri, r)),
                        })
                        .collect())
                } else {
                    let calls: Option<Vec<CallHierarchyOutgoingCall>> = client
                        .request("callHierarchy/outgoingCalls", Some(follow_params))
                        .await?;

                    Ok(calls
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| CallHierarchyItem {
                            name: c.to.name,
                            kind: convert_symbol_kind(c.to.kind),
                            location: uri_range_to_location(&c.to.uri, &c.to.selection_range),
                            call_site: c
                                .from_ranges
                                .first()
                                .map(|r| uri_range_to_location(&uri, r)),
                        })
                        .collect())
                }
            }
        })
        .await
}

pub(super) async fn supertypes(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Vec<TypeHierarchyItem>, LspError> {
    type_hierarchy(service, file, line, column, "typeHierarchy/supertypes").await
}

pub(super) async fn subtypes(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Vec<TypeHierarchyItem>, LspError> {
    type_hierarchy(service, file, line, column, "typeHierarchy/subtypes").await
}

async fn type_hierarchy(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
    method: &str,
) -> Result<Vec<TypeHierarchyItem>, LspError> {
    check_feature_support(file, LspFeature::TypeHierarchy)?;

    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);
    let method = method.to_string();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            let method = method.clone();
            async move {
                ensure_indexed(&client, &file, manager.root()).await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let prepare_params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column)
                });

                let items: Option<Vec<serde_json::Value>> = client
                    .request("textDocument/prepareTypeHierarchy", Some(prepare_params))
                    .await?;

                let items = items.unwrap_or_default();
                if items.is_empty() {
                    return Ok(vec![]);
                }

                let follow_params = serde_json::json!({ "item": items.first().unwrap() });

                let results: Option<Vec<serde_json::Value>> =
                    client.request(&method, Some(follow_params)).await?;

                Ok(results
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|item| parse_type_hierarchy_item(&item))
                    .collect())
            }
        })
        .await
}
