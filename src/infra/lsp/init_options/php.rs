use serde_json::{Value, json};
pub(super) fn php_init_options() -> Value {
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
            "exclude": [
                "**/.git/**",
                "**/.svn/**",
                "**/node_modules/**",
                "**/vendor/**/{Tests,tests}/**",
                "**/storage/**",
                "**/cache/**"
            ]
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
