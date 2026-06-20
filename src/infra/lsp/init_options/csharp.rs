use std::path::Path;

use serde_json::{Value, json};

use super::exclude::lsp_exclude_search_patterns;

pub(super) fn csharp_init_options(root: &Path) -> Value {
    json!({
        "RoslynExtensionsOptions": {
            "enableAnalyzersSupport": true,
            "enableImportCompletion": true,
            "enableDecompilationSupport": true,
            "enableAsyncCompletion": false,
            "documentAnalysisTimeoutMs": 30000,
            "diagnosticWorkersThreadCount": 8,
            "analyzeOpenDocumentsOnly": true,
            "inlayHintsOptions": {
                "enableForParameters": false,
                "forLiteralParameters": false,
                "forObjectCreationParameters": false,
                "enableForTypes": false,
                "forImplicitVariableTypes": false,
                "forLambdaParameterTypes": false,
                "forImplicitObjectCreation": false
            },
            "locationPaths": null
        },
        "FormattingOptions": {
            "enableEditorConfigSupport": true,
            "organizeImports": false,
            "newLine": "
    ",
            "useTabs": false,
            "tabSize": 4,
            "indentationSize": 4
        },
        "FileOptions": {
            // C#-specific build/system paths that should never be indexed,
            // independent of the project's ignore policy.
            "systemExcludeSearchPatterns": [
                "**/bin/**/*",
                "**/obj/**/*"
            ],
            // Derived from the native-index ignore policy so OmniSharp and the
            // index agree on which files exist (see init_options/exclude.rs).
            "excludeSearchPatterns": lsp_exclude_search_patterns(root)
        },
        "RenameOptions": {
            "renameInComments": false,
            "renameInStrings": false,
            "renameOverloads": true
        },
        "ImplementTypeOptions": {
            "insertionBehavior": "WithOtherMembersOfTheSameKind",
            "propertyGenerationBehavior": "PreferAutoProperties"
        },
        "DotNetCliOptions": {
            "locationPaths": null
        },
        "Plugins": {
            "locationPaths": null
        }
    })
}
