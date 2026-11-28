use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspFeature;
use crate::infra::lsp::protocol::{
    LspLocation, TextDocumentIdentifier, TextDocumentPositionParams,
};
use crate::models::lsp::{Definition, Indexed, path_to_uri};
use crate::models::symbol::Location;

use super::converters::*;
use super::helpers::*;
use super::position::PositionConverter;
use super::service::{DefaultLspService, ensure_indexed};

pub(super) async fn find_references(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Vec<Location>>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);
    let project_root = manager.root().to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            let project_root = project_root.clone();
            async move {

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;
                let waited = ensure_indexed(&client, &file, manager.root()).await;
                let indexing = client.indexing_query(waited);

                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column, &content, client.position_encoding().await),
                    "context": { "includeDeclaration": true }
                });

                let result: serde_json::Value = client
                    .request("textDocument/references", Some(params))
                    .await
                    .map_err(|err| {
                        unsupported_if_unimplemented(err, &manager, &file, LspFeature::FindReferences)
                    })?;

                if result.is_null() {
                    return Ok(indexing.finish(Vec::new()));
                }

                let locations: Vec<LspLocation> = serde_json::from_value(result)
                    .map_err(|e| LspError::Protocol(e.to_string()))?;

                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);
                let all_locations: Vec<Location> =
                    locations.iter().map(|l| convert_location(l, &mut conv)).collect();

                Ok(indexing.finish(
                    filter_locations_within_project(all_locations, &project_root)
                ))
            }
        })
        .await
}

pub(super) async fn goto_definition(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Option<Definition>>, LspError> {
    let named = goto_locations(
        service,
        file,
        line,
        column,
        "textDocument/definition",
        LspFeature::GotoDefinition,
    )
    .await?;
    let best = select_best_definition(
        named.data.iter().map(|(wire, _)| wire.uri.as_str()),
        service.manager.root(),
    );
    let definition = best.map(|index| {
        let (wire, location) = &named.data[index];
        // The server said the queried token is its own definition when the
        // target starts inside the origin token it identified at the query
        // position. A plain-Location server names no origin; the one thing
        // it can still say that way is a definition of the exact queried
        // position to itself. Same-file is judged on the PARSED path — a
        // raw URI comparison would split on percent-encoding choices the
        // two sides are free to make differently (`(`, `,`, `+`, …).
        let is_self = location.file == file
            && match &wire.origin {
                Some(origin) => {
                    (origin.start.line, origin.start.character)
                        <= (wire.range.start.line, wire.range.start.character)
                        && (wire.range.start.line, wire.range.start.character)
                            < (origin.end.line, origin.end.character)
                }
                None => (location.line, location.column) == (line, column),
            };
        Definition {
            location: location.clone(),
            is_self,
            candidates: named.data.iter().map(|(_, named)| named.clone()).collect(),
        }
    });
    Ok(Indexed::new(definition, named.indexing))
}

pub(super) async fn goto_type_definition(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Vec<Location>>, LspError> {
    let named = goto_locations(
        service,
        file,
        line,
        column,
        "textDocument/typeDefinition",
        LspFeature::GotoTypeDefinition,
    )
    .await?;
    // A composite type names one definition per type it is built from
    // (rust-analyzer answers `Vec<Foo>` with `Vec`, `Foo` and `Global`), so
    // the first alone would drop the project's own type behind a library's.
    let mut types: Vec<Location> = Vec::with_capacity(named.data.len());
    for (_, location) in named.data {
        if !types.contains(&location) {
            types.push(location);
        }
    }
    Ok(Indexed::new(types, named.indexing))
}

/// Every location a navigation request names at a position, in the server's
/// order: as the server sent it, which keeps the origin a `LocationLink`
/// names, and decoded.
async fn goto_locations(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
    method: &'static str,
    feature: LspFeature,
) -> Result<Indexed<Vec<(LspLocation, Location)>>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;
                let waited = ensure_indexed(&client, &file, manager.root()).await;
                let indexing = client.indexing_query(waited);

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(
                        line,
                        column,
                        &content,
                        client.position_encoding().await,
                    ),
                };

                let result: serde_json::Value = client
                    .request(method, Some(serde_json::to_value(params)?))
                    .await
                    .map_err(|err| unsupported_if_unimplemented(err, &manager, &file, feature))?;

                // Each target is decoded from the wire encoding here, where the
                // client (and so the encoding) is in scope.
                let mut conv = PositionConverter::new(client.position_encoding().await);
                let named = parse_location_response(result)?
                    .into_iter()
                    .map(|loc| {
                        let decoded = convert_location(&loc, &mut conv);
                        (loc, decoded)
                    })
                    .collect();
                Ok(indexing.finish(named))
            }
        })
        .await
}

pub(super) async fn find_implementations(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Vec<Location>>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                check_feature_support_live(
                    &manager,
                    &client,
                    &file,
                    LspFeature::FindImplementations,
                )
                .await?;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;
                let waited = ensure_indexed(&client, &file, manager.root()).await;
                let indexing = client.indexing_query(waited);

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(
                        line,
                        column,
                        &content,
                        client.position_encoding().await,
                    ),
                };

                let result: serde_json::Value = client
                    .request(
                        "textDocument/implementation",
                        Some(serde_json::to_value(params)?),
                    )
                    .await
                    .map_err(|err| {
                        unsupported_if_unimplemented(
                            err,
                            &manager,
                            &file,
                            LspFeature::FindImplementations,
                        )
                    })?;

                let locs = parse_location_response(result)?;
                // Post-flight null-gap: an empty implementation result from a
                // server that doesn't advertise the provider is unsupported, not
                // "no implementations".
                if locs.is_empty()
                    && !client
                        .feature_advertised(LspFeature::FindImplementations)
                        .await
                {
                    return Err(unsupported_error(
                        &manager,
                        &file,
                        LspFeature::FindImplementations,
                    ));
                }

                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);
                Ok(indexing.finish(
                    locs.iter()
                        .map(|l| convert_location(l, &mut conv))
                        .collect(),
                ))
            }
        })
        .await
}
