use serde_json::{Value, json};
pub(super) fn csharp_init_options() -> Value {
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
            "systemExcludeSearchPatterns": [
                "**/node_modules/**/*",
                "**/bin/**/*",
                "**/obj/**/*",
                "**/.git/**/*"
            ],
            "excludeSearchPatterns": []
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
