use std::path::Path;

use serde_json::{Value, json};

use crate::models::lsp::path_to_uri;
pub(super) fn kotlin_init_options(root_path: &Path) -> Value {
    let root_uri = path_to_uri(root_path);

    // Enable server-side caching for faster subsequent operations
    let storage_path = dirs::cache_dir()
        .map(|p| p.join("symora").join("kotlin-ls"))
        .and_then(|p| {
            // Ensure directory exists
            let _ = std::fs::create_dir_all(&p);
            p.to_str().map(|s| s.to_string())
        });

    json!({
        "workspaceFolders": [root_uri],
        "storagePath": storage_path,
        "codegen": {
            "enabled": false
        },
        "compiler": {
            "jvm": {
                "target": "17"
            }
        },
        "completion": {
            "snippets": {
                "enabled": true
            }
        },
        "diagnostics": {
            "enabled": true,
            "level": 4,
            "debounceTime": 250
        },
        "scripts": {
            "enabled": true,
            "buildScriptsEnabled": true
        },
        "indexing": {
            "enabled": true
        },
        "externalSources": {
            "useKlsScheme": false,
            "autoConvertToKotlin": false
        },
        "inlayHints": {
            "typeHints": false,
            "parameterHints": false,
            "chainedHints": false
        },
        "formatting": {
            "formatter": "ktfmt",
            "ktfmt": {
                "style": "google",
                "indent": 4,
                "maxWidth": 100,
                "continuationIndent": 8,
                "removeUnusedImports": true
            }
        }
    })
}
