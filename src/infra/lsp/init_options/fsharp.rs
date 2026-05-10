use std::path::Path;

use serde_json::{Value, json};

use crate::models::lsp::path_to_uri;
pub(super) fn fsharp_init_options(root_path: &Path) -> Value {
    let root_uri = path_to_uri(root_path);
    json!({
        "automaticWorkspaceInit": true,
        "workspacePath": root_uri,
        "workspaceModePeekDeepLevel": 2,
        "abstractClassStubGeneration": true,
        "abstractClassStubGenerationObjectIdentifier": "this",
        "abstractClassStubGenerationMethodBody": "failwith \"Not Implemented\"",
        "addPrivateAccessModifier": false,
        "unusedOpensAnalyzer": true,
        "unusedDeclarationsAnalyzer": true,
        "simplifyNameAnalyzer": true,
        "resolveNamespaces": true,
        "enableReferenceCodeLens": true,
        "dotNetRoot": null,
        "fsiExtraParameters": [],
        "linter": true,
        "indentationSize": 4,
        "interfaceStubGeneration": true,
        "pipelineHints": {
            "enabled": true
        },
        "fsac": {
            "cachedTypeCheckCount": 200,
            "conserveMemory": true,
            "silencedLogs": [],
            "analyzersPath": [],
            "sourceTextImplementation": "NamedText"
        },
        "codeLenses": {
            "signature": {
                "enabled": true
            },
            "references": {
                "enabled": true
            }
        },
        "inlayHints": {
            "enabled": false,
            "typeAnnotations": false,
            "parameterNames": false,
            "disableLongTooltip": true
        },
        "debug": {
            "dontCheckRelatedFiles": false,
            "checkFileDebouncerTimeout": 250
        }
    })
}
