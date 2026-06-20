use std::path::Path;

use serde_json::{Value, json};

use super::exclude::lsp_exclude_subtree_globs;

pub(super) fn ruby_init_options(root: &Path) -> Value {
    // Policy-derived dependency/build dirs (so ruby-lsp's index and Symora's
    // agree) ∪ Rails dirs holding generated/compiled assets — server-specific,
    // not an ignore-policy concern.
    let mut excluded = lsp_exclude_subtree_globs(root);
    excluded.extend(
        [
            "**/public/assets/**",
            "**/public/packs/**",
            "**/public/webpack/**",
            "**/app/assets/builds/**",
            "**/storage/**",
            "**/log/**",
            "**/doc/**",
        ]
        .into_iter()
        .map(str::to_string),
    );
    excluded.sort();
    excluded.dedup();
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
            "excludedPatterns": excluded
        },
        "experimentalFeaturesEnabled": false
    })
}
