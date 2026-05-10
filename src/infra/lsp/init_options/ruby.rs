use serde_json::{Value, json};
pub(super) fn ruby_init_options() -> Value {
    json!({
        "enabledFeatures": {
            "codeActions": true,
            "diagnostics": true,
            "documentHighlights": true,
            "documentLink": true,
            "documentSymbols": true,
            "foldingRanges": true,
            "formatting": true,
            "hover": true,
            "inlayHint": false,
            "onTypeFormatting": true,
            "selectionRanges": true,
            "semanticHighlighting": true,
            "completion": true,
            "codeLens": true,
            "definition": true,
            "workspaceSymbol": true,
            "signatureHelp": true,
            "typeHierarchy": true
        },
        "formatter": "auto",
        "linters": ["rubocop"],
        "rubyVersionManager": "auto",
        "indexing": {
            "includedPatterns": ["**/*.rb", "**/*.rake", "**/*.ru", "**/*.erb"],
            "excludedPatterns": [
                // Standard exclusions
                "**/vendor/**",
                "**/.bundle/**",
                "**/tmp/**",
                "**/log/**",
                "**/coverage/**",
                "**/.yardoc/**",
                "**/doc/**",
                "**/.git/**",
                "**/node_modules/**",
                // Rails-specific exclusions
                "**/public/assets/**",
                "**/public/packs/**",
                "**/public/webpack/**",
                "**/app/assets/builds/**",
                "**/storage/**"
            ]
        },
        "experimentalFeaturesEnabled": false
    })
}
