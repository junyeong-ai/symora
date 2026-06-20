use std::path::Path;

use serde_json::{Value, json};

use super::exclude::lsp_exclude_go_directory_filters;

pub(super) fn go_init_options(root: &Path) -> Value {
    json!({
        "usePlaceholders": true,
        "completionDocumentation": true,
        "deepCompletion": true,
        "completeUnimported": true,
        "staticcheck": true,
        "gofumpt": false,
        "semanticTokens": true,
        "memoryMode": "DegradeClosed",
        // Derived from the native-index ignore policy so gopls and the index
        // agree on which files exist (see init_options/exclude.rs). gopls skips
        // dotted dirs and testdata itself, so the policy need not name them.
        "directoryFilters": lsp_exclude_go_directory_filters(root),
        "analyses": {
            "unusedparams": true,
            "shadow": true,
            "fieldalignment": false,
            "nilness": true,
            "unusedwrite": true,
            "useany": true,
            "unusedvariable": true
        },
        "hints": {
            "assignVariableTypes": false,
            "compositeLiteralFields": false,
            "compositeLiteralTypes": false,
            "constantValues": false,
            "functionTypeParameters": false,
            "parameterNames": false,
            "rangeVariableTypes": false
        },
        "codelenses": {
            "gc_details": true,
            "generate": true,
            "regenerate_cgo": true,
            "run_govulncheck": true,
            "test": true,
            "tidy": true,
            "upgrade_dependency": true,
            "vendor": true
        }
    })
}
