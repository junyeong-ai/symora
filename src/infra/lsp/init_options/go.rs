use serde_json::{Value, json};
pub(super) fn go_init_options() -> Value {
    json!({
        "usePlaceholders": true,
        "completionDocumentation": true,
        "deepCompletion": true,
        "completeUnimported": true,
        "staticcheck": true,
        "gofumpt": false,
        "semanticTokens": true,
        "memoryMode": "DegradeClosed",
        "directoryFilters": ["-**/vendor", "-**/node_modules", "-**/.git", "-**/testdata"],
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
