//! The capabilities this client advertises in `initialize`: what it
//! implements, per language, and nothing it would not honour.

use crate::models::symbol::Language;

use crate::infra::lsp::protocol::{
    ClientCapabilities, GeneralClientCapabilities, RegularExpressionsCapability,
    StaleRequestSupport, TextDocumentClientCapabilities, WindowClientCapabilities,
    WorkspaceClientCapabilities,
};

/// Whether `language`'s server is offered pull diagnostics.
///
/// A pulled report is read as the server's analysis of the document's
/// current content. pyright answers a pull once it has analyzed the
/// document, and rust-analyzer once its workspace status reports the
/// workspace loaded, for a file in a project it names
/// (`LspClient::pull_diagnostics`). sourcekit-lsp answers with an empty
/// report until its build settings load. A server offered pulls may stop
/// publishing, so every other server stays on publishes, which it sends
/// once it has analyzed the document.
pub(super) fn offers_pull_diagnostics(language: Language) -> bool {
    matches!(language, Language::Python | Language::Rust)
}

/// Negotiate static providers and the client's implemented dynamic registrations.
pub(super) fn client_capabilities(language: Language, file_watching: bool) -> ClientCapabilities {
    let general = GeneralClientCapabilities {
        position_encodings: Some(vec!["utf-8".to_string(), "utf-16".to_string()]),
        stale_request_support: Some(StaleRequestSupport {
            cancel: true,
            retry_on_content_modified: Some(vec![
                "textDocument/semanticTokens/full".to_string(),
                "textDocument/semanticTokens/range".to_string(),
                "textDocument/semanticTokens/full/delta".to_string(),
            ]),
        }),
        regular_expressions: Some(RegularExpressionsCapability {
            engine: "ECMAScript".to_string(),
            version: Some("ES2020".to_string()),
        }),
    };

    let window = WindowClientCapabilities {
        work_done_progress: Some(true),
        show_message: Some(serde_json::json!({
            "messageActionItem": {
                "additionalPropertiesSupport": true
            }
        })),
        show_document: Some(serde_json::json!({
            "support": true
        })),
    };

    let text_document = TextDocumentClientCapabilities {
        synchronization: Some(serde_json::json!({
            "dynamicRegistration": false,
            "willSave": true,
            "willSaveWaitUntil": true,
            "didSave": true
        })),
        completion: Some(completion_capabilities(language)),
        hover: Some(serde_json::json!({
            "dynamicRegistration": false,
            "contentFormat": ["markdown", "plaintext"]
        })),
        signature_help: Some(serde_json::json!({
            "dynamicRegistration": false,
            "signatureInformation": {
                "documentationFormat": ["markdown", "plaintext"],
                "parameterInformation": { "labelOffsetSupport": true },
                "activeParameterSupport": true
            },
            "contextSupport": true
        })),
        declaration: Some(serde_json::json!({
            "dynamicRegistration": false,
            "linkSupport": true
        })),
        definition: Some(serde_json::json!({
            "dynamicRegistration": false,
            "linkSupport": true
        })),
        type_definition: Some(serde_json::json!({
            "dynamicRegistration": false,
            "linkSupport": true
        })),
        implementation: Some(serde_json::json!({
            "dynamicRegistration": false,
            "linkSupport": true
        })),
        references: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        document_highlight: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        document_symbol: Some(serde_json::json!({
            "dynamicRegistration": false,
            "symbolKind": {
                "valueSet": (1..=26).collect::<Vec<_>>()
            },
            "hierarchicalDocumentSymbolSupport": true,
            "tagSupport": { "valueSet": [1] },
            "labelSupport": true
        })),
        code_action: Some(serde_json::json!({
            "dynamicRegistration": false,
            "isPreferredSupport": true,
            "disabledSupport": true,
            "dataSupport": true,
            "resolveSupport": {
                "properties": ["edit"]
            },
            "codeActionLiteralSupport": {
                "codeActionKind": {
                    "valueSet": [
                        "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                        "refactor.rewrite", "source", "source.organizeImports", "source.fixAll"
                    ]
                }
            },
            "honorsChangeAnnotations": true
        })),
        code_lens: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        document_link: Some(serde_json::json!({
            "dynamicRegistration": false,
            "tooltipSupport": true
        })),
        color_provider: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        formatting: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        range_formatting: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        on_type_formatting: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        rename: Some(serde_json::json!({
            "dynamicRegistration": false,
            "prepareSupport": true,
            "prepareSupportDefaultBehavior": 1,
            "honorsChangeAnnotations": true
        })),
        publish_diagnostics: Some(serde_json::json!({
            "relatedInformation": true,
            "tagSupport": { "valueSet": [1, 2] },
            "versionSupport": true,
            "codeDescriptionSupport": true,
            "dataSupport": true
        })),
        folding_range: Some(serde_json::json!({
            "dynamicRegistration": false,
            "rangeLimit": 5000,
            "lineFoldingOnly": false,
            "foldingRangeKind": {
                "valueSet": ["comment", "imports", "region"]
            },
            "foldingRange": { "collapsedText": true }
        })),
        selection_range: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        linked_editing_range: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        call_hierarchy: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        semantic_tokens: Some(serde_json::json!({
            "dynamicRegistration": false,
            "requests": {
                "range": true,
                "full": { "delta": true }
            },
            "tokenTypes": [
                "namespace", "type", "class", "enum", "interface", "struct", "typeParameter",
                "parameter", "variable", "property", "enumMember", "event", "function",
                "method", "macro", "keyword", "modifier", "comment", "string", "number",
                "regexp", "operator", "decorator"
            ],
            "tokenModifiers": [
                "declaration", "definition", "readonly", "static", "deprecated", "abstract",
                "async", "modification", "documentation", "defaultLibrary"
            ],
            "formats": ["relative"],
            "overlappingTokenSupport": false,
            "multilineTokenSupport": true,
            "serverCancelSupport": true,
            "augmentsSyntaxTokens": true
        })),
        moniker: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        type_hierarchy: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        inline_value: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        inlay_hint: Some(serde_json::json!({
            "dynamicRegistration": false,
            "resolveSupport": {
                "properties": ["tooltip", "textEdits", "label.tooltip", "label.location", "label.command"]
            }
        })),
        diagnostic: offers_pull_diagnostics(language).then(|| {
            serde_json::json!({
                "dynamicRegistration": true,
                "relatedDocumentSupport": false
            })
        }),
    };

    let workspace = WorkspaceClientCapabilities {
        apply_edit: Some(true),
        workspace_edit: Some(serde_json::json!({
            "documentChanges": true,
            "resourceOperations": ["create", "rename", "delete"],
            "failureHandling": "textOnlyTransactional",
            "normalizesLineEndings": true,
            "changeAnnotationSupport": { "groupsOnLabel": true }
        })),
        did_change_configuration: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        did_change_watched_files: file_watching.then(|| {
            serde_json::json!({
                "dynamicRegistration": true,
                "relativePatternSupport": true
            })
        }),
        symbol: Some(serde_json::json!({
            "dynamicRegistration": false,
            "symbolKind": {
                "valueSet": (1..=26).collect::<Vec<_>>()
            },
            "tagSupport": { "valueSet": [1] },
            "resolveSupport": { "properties": ["location.range"] }
        })),
        execute_command: Some(serde_json::json!({
            "dynamicRegistration": false
        })),
        workspace_folders: Some(true),
        configuration: Some(true),
        semantic_tokens: Some(serde_json::json!({
            "refreshSupport": true
        })),
        code_lens: Some(serde_json::json!({
            "refreshSupport": true
        })),
        file_operations: Some(serde_json::json!({
            "dynamicRegistration": false,
            "didCreate": true,
            "willCreate": true,
            "didRename": true,
            "willRename": true,
            "didDelete": true,
            "willDelete": true
        })),
        inline_value: Some(serde_json::json!({
            "refreshSupport": true
        })),
        inlay_hint: Some(serde_json::json!({
            "refreshSupport": true
        })),
        diagnostics: None,
    };

    ClientCapabilities {
        general: Some(general),
        window: Some(window),
        text_document: Some(text_document),
        workspace: Some(workspace),
        // `serverStatusNotification` asks rust-analyzer to send
        // `experimental/serverStatus`. Healthy quiescence confirms
        // readiness. Other servers ignore unknown experimental keys.
        experimental: Some(serde_json::json!({
            "serverStatusNotification": true
        })),
    }
}

/// Build completion capabilities based on language
fn completion_capabilities(language: Language) -> serde_json::Value {
    let snippet_support = !matches!(language, Language::Kotlin);

    serde_json::json!({
        "dynamicRegistration": false,
        "contextSupport": true,
        "completionItem": {
            "snippetSupport": snippet_support,
            "commitCharactersSupport": true,
            "documentationFormat": ["markdown", "plaintext"],
            "deprecatedSupport": true,
            "preselectSupport": true,
            "tagSupport": { "valueSet": [1] },
            "insertReplaceSupport": false,
            "resolveSupport": {
                "properties": ["documentation", "detail", "additionalTextEdits"]
            },
            "insertTextModeSupport": { "valueSet": [1, 2] },
            "labelDetailsSupport": true
        },
        "insertTextMode": 2,
        "completionItemKind": {
            "valueSet": (1..=25).collect::<Vec<_>>()
        },
        "completionList": {
            "itemDefaults": ["commitCharacters", "editRange", "insertTextFormat", "insertTextMode"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn file_watching_is_offered_only_when_asked() {
        let offered = |watching| {
            client_capabilities(Language::Python, watching)
                .workspace
                .and_then(|workspace| workspace.did_change_watched_files)
                .is_some()
        };
        assert!(offered(true));
        assert!(!offered(false));
    }

    #[test]
    fn dynamic_registration_is_offered_for_watching_and_pulling_only() {
        fn offered(value: &Value, path: &str, dynamic: &mut Vec<String>) {
            if let Some(object) = value.as_object() {
                for (key, value) in object {
                    let path = format!("{path}.{key}");
                    if key == "dynamicRegistration" && value == &Value::Bool(true) {
                        dynamic.push(path);
                    } else {
                        offered(value, &path, dynamic);
                    }
                }
            }
        }
        for language in Language::all() {
            for watching in [false, true] {
                let capabilities =
                    serde_json::to_value(client_capabilities(language, watching)).unwrap();
                let mut dynamic = Vec::new();
                offered(&capabilities, "", &mut dynamic);
                let mut expected = Vec::new();
                if offers_pull_diagnostics(language) {
                    expected.push(".textDocument.diagnostic.dynamicRegistration");
                }
                if watching {
                    expected.push(".workspace.didChangeWatchedFiles.dynamicRegistration");
                }
                assert_eq!(dynamic, expected, "{language:?}");
            }
        }
    }
}
