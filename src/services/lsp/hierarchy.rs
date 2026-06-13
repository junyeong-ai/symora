use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspFeature;
use crate::infra::lsp::protocol::{
    CallHierarchyIncomingCall, CallHierarchyOutgoingCall, LspCallHierarchyItem,
};
use crate::models::lsp::{CallHierarchyItem, Indexed, TypeHierarchyItem, path_to_uri};

use super::converters::*;
use super::helpers::*;
use super::position::PositionConverter;
use super::service::{DefaultLspService, degradation_of, ensure_indexed};

pub(super) async fn incoming_calls(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
    call_hierarchy(service, file, line, column, true).await
}

pub(super) async fn outgoing_calls(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
    call_hierarchy(service, file, line, column, false).await
}

async fn call_hierarchy(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
    incoming: bool,
) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
    let feature = if incoming {
        LspFeature::IncomingCalls
    } else {
        LspFeature::OutgoingCalls
    };

    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                // Pre-flight: the static table only hints; if it says None but
                // this server advertises call hierarchy, attempt the request.
                check_feature_support_live(&client, &file, feature).await?;
                ensure_indexed(&client, &file, manager.root()).await;
                client.sleep_for_cross_file_settle().await;
                // Computation-time snapshot — the output marker derives
                // from the state the query was issued under.
                let ran_under = degradation_of(client.indexing_state());

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let prepare_params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column, &content, client.position_encoding().await)
                });

                let items: Option<Vec<LspCallHierarchyItem>> = client
                    .request("textDocument/prepareCallHierarchy", Some(prepare_params))
                    .await?;

                let items = items.unwrap_or_default();
                if items.is_empty() {
                    // Post-flight: an empty prepare from a server that does NOT
                    // advertise the provider is a null-gap, not a real "no
                    // callers" — surface unsupported so callers fall back to
                    // references-derived. If it IS advertised, the empty is
                    // genuine and returned as-is.
                    if !client.feature_advertised(feature).await {
                        return Err(unsupported_error(&file, feature));
                    }
                    return Ok(Indexed::new(vec![], ran_under));
                }

                let follow_params = serde_json::json!({ "item": items.first().unwrap() });

                // Caller/callee positions arrive in the negotiated wire
                // encoding; decode them to native scalar columns, caching each
                // target file's lines (the anchor file is seeded).
                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);

                if incoming {
                    let calls: Option<Vec<CallHierarchyIncomingCall>> = client
                        .request("callHierarchy/incomingCalls", Some(follow_params))
                        .await?;

                    Ok(Indexed::new(
                        calls
                            .unwrap_or_default()
                            .into_iter()
                            .map(|c| CallHierarchyItem {
                                name: c.from.name,
                                kind: convert_symbol_kind(c.from.kind),
                                location: uri_range_to_location(
                                    &c.from.uri,
                                    &c.from.selection_range,
                                    &mut conv,
                                ),
                                call_site: c
                                    .from_ranges
                                    .first()
                                    .map(|r| uri_range_to_location(&c.from.uri, r, &mut conv)),
                            })
                            .collect(),
                        ran_under,
                    ))
                } else {
                    let calls: Option<Vec<CallHierarchyOutgoingCall>> = client
                        .request("callHierarchy/outgoingCalls", Some(follow_params))
                        .await?;

                    Ok(Indexed::new(
                        calls
                            .unwrap_or_default()
                            .into_iter()
                            .map(|c| CallHierarchyItem {
                                name: c.to.name,
                                kind: convert_symbol_kind(c.to.kind),
                                location: uri_range_to_location(
                                    &c.to.uri,
                                    &c.to.selection_range,
                                    &mut conv,
                                ),
                                call_site: c
                                    .from_ranges
                                    .first()
                                    .map(|r| uri_range_to_location(&uri, r, &mut conv)),
                            })
                            .collect(),
                        ran_under,
                    ))
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
) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
    type_hierarchy(service, file, line, column, "typeHierarchy/supertypes").await
}

pub(super) async fn subtypes(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
    type_hierarchy(service, file, line, column, "typeHierarchy/subtypes").await
}

async fn type_hierarchy(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
    method: &str,
) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
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
                check_feature_support_live(&client, &file, LspFeature::TypeHierarchy).await?;
                ensure_indexed(&client, &file, manager.root()).await;
                let ran_under = degradation_of(client.indexing_state());

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let prepare_params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column, &content, client.position_encoding().await)
                });

                let items: Option<Vec<serde_json::Value>> = client
                    .request("textDocument/prepareTypeHierarchy", Some(prepare_params))
                    .await?;

                let items = items.unwrap_or_default();
                if items.is_empty() {
                    // Post-flight null-gap: a server that doesn't advertise type
                    // hierarchy returning empty is unsupported, not "no types".
                    if !client.feature_advertised(LspFeature::TypeHierarchy).await {
                        return Err(unsupported_error(&file, LspFeature::TypeHierarchy));
                    }
                    return Ok(Indexed::new(vec![], ran_under));
                }

                let follow_params = serde_json::json!({ "item": items.first().unwrap() });

                let results: Option<Vec<serde_json::Value>> =
                    client.request(&method, Some(follow_params)).await?;

                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);
                Ok(Indexed::new(
                    results
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|item| parse_type_hierarchy_item(&item, &mut conv))
                        .collect(),
                    ran_under,
                ))
            }
        })
        .await
}
