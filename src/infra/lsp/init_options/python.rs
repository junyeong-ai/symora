use serde_json::{Value, json};
pub(super) fn python_init_options() -> Value {
    json!({
        "python": {
            "analysis": {
                "autoSearchPaths": true,
                "useLibraryCodeForTypes": true,
                "diagnosticMode": "openFilesOnly",
                "typeCheckingMode": "off",
                "autoImportCompletions": false,
                "indexing": true,
                "logLevel": "Warning",
                "exclude": [
                    "**/__pycache__",
                    "**/.venv",
                    "**/.env",
                    "**/build",
                    "**/dist",
                    "**/.pixi",
                    "**/venv",
                    "**/.tox",
                    "**/.nox",
                    "**/.mypy_cache",
                    "**/.pytest_cache",
                    "**/node_modules",
                    "**/.git",
                    "**/site-packages",
                    "**/.eggs",
                    "**/htmlcov",
                    "**/*.egg-info",
                    "**/migrations",
                    "**/target",
                    "**/vendor"
                ],
                "diagnosticSeverityOverrides": {
                    "reportMissingImports": "none",
                    "reportMissingTypeStubs": "none",
                    "reportPrivateUsage": "none",
                    "reportUntypedBaseClass": "none",
                    "reportUnusedImport": "none",
                    "reportUnusedVariable": "none",
                    "reportGeneralTypeIssues": "none"
                }
            }
        }
    })
}
