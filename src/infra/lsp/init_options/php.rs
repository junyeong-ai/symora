use std::path::Path;

use serde_json::{Value, json};

use super::exclude::lsp_exclude_subtree_globs;

pub(super) fn php_init_options(root: &Path) -> Value {
    // Policy-derived dependency dirs (so intelephense and the index agree) ∪
    // Laravel runtime dirs that hold generated PHP — compiled Blade views,
    // framework cache — server-specific, not an ignore-policy concern.
    let mut exclude = lsp_exclude_subtree_globs(root);
    exclude.extend(
        ["**/storage/**", "**/cache/**"]
            .into_iter()
            .map(str::to_string),
    );
    exclude.sort();
    exclude.dedup();
    json!({
        "clearCache": false,
        "globalStoragePath": null,
        "storagePath": null,
        "maxMemory": 4096,
        "environment": {
            "includePaths": []
        },
        "files": {
            "maxSize": 5000000,
            "exclude": exclude
        },
        "stubs": [
            "apache", "bcmath", "bz2", "calendar", "Core", "ctype", "curl",
            "date", "dom", "fileinfo", "filter", "gd", "hash", "iconv",
            "intl", "json", "libxml", "mbstring", "mcrypt", "mysqli",
            "openssl", "pcre", "PDO", "pdo_mysql", "Phar", "posix",
            "readline", "Reflection", "regex", "session", "SimpleXML",
            "soap", "sockets", "sodium", "SPL", "sqlite3", "standard",
            "tokenizer", "xml", "xmlreader", "xmlwriter", "zip", "zlib"
        ],
        "completion": {
            "insertUseDeclaration": true,
            "fullyQualifyGlobalConstantsAndFunctions": false,
            "triggerParameterHints": true,
            "maxItems": 100
        },
        "format": {
            "enable": true
        },
        "diagnostics": {
            "enable": true,
            "run": "onType"
        }
    })
}
