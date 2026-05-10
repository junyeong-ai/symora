use serde_json::{Value, json};
pub(super) fn typescript_init_options() -> Value {
    json!({
        "hostInfo": "symora",
        "preferences": {
            "includeInlayParameterNameHints": "none",
            "includeInlayPropertyDeclarationTypeHints": false,
            "includeInlayFunctionLikeReturnTypeHints": false,
            "includeInlayVariableTypeHints": false,
            "importModuleSpecifierPreference": "shortest",
            "includePackageJsonAutoImports": "auto",
            "quotePreference": "auto",
            "allowIncompleteCompletions": true,
            "allowRenameOfImportPath": true,
            "displayPartsForJSDoc": true,
            "providePrefixAndSuffixTextForRename": true,
            "autoImportFileExcludePatterns": [
                "**/node_modules/@types/node/**",
                "**/.git/**"
            ]
        },
        "tsserver": {
            "logVerbosity": "off",
            "maxTsServerMemory": 4096
        },
        "implicitProjectConfiguration": {
            "checkJs": false,
            "strictNullChecks": true,
            "target": "ES2022",
            "module": "NodeNext"
        }
    })
}
