use serde_json::{Value, json};

pub(super) fn elixir_init_options() -> Value {
    json!({
        "autoBuild": true,
        "mixEnv": "test",
        "mixTarget": "host",
        "fetchDeps": false,
        "dialyzerEnabled": true,
        "incrementalDialyzer": true,
        "suggestSpecs": true,
        "enableTestLenses": false,
        "autoInsertRequiredAlias": true,
        "signatureAfterComplete": true,
        "dialyzerFormat": "dialyzer",
        "projectDir": null
    })
}

pub(super) fn haskell_init_options() -> Value {
    json!({
        "haskell": {
            "checkProject": false,
            "checkParents": "CheckOnSave",
            "sessionLoading": "singleComponent",
            "maxCompletions": 40,
            "formattingProvider": "ormolu",
            "cabalFormattingProvider": "cabal-gild",
            "plugin": {
                "hlint": {
                    "globalOn": true,
                    "diagnosticsOn": true,
                    "codeActionsOn": true
                },
                "pragmas": {
                    "globalOn": true,
                    "codeActionsOn": true
                },
                "ghcide-completions": {
                    "config": {
                        "autoExtendOn": true
                    }
                },
                "ghcide-type-lenses": {
                    "config": {
                        "mode": "always"
                    }
                },
                "eval": {
                    "globalOn": true,
                    "codeLensOn": false
                },
                "rename": {
                    "globalOn": true
                }
            }
        }
    })
}

pub(super) fn dart_init_options() -> Value {
    json!({
        "closingLabels": true,
        "outline": true,
        "flutterOutline": true,
        "suggestFromUnimportedLibraries": true,
        "completeFunctionCalls": true,
        "enableSnippets": true,
        "updateImportsOnRename": true,
        "documentation": "full",
        "includeDependenciesInWorkspaceSymbols": true,
        "enableSdkFormatter": true,
        "lineLength": 80,
        "showTodos": false,
        "renameFilesWithClasses": "prompt",
        "analysisExcludedFolders": [
            ".dart_tool",
            ".idea",
            "build",
            ".pub-cache"
        ]
    })
}

pub(super) fn nix_init_options() -> Value {
    json!({
        "nixpkgs": {
            "expr": "import <nixpkgs> { }"
        },
        "formatting": {
            "command": ["nixpkgs-fmt"]
        },
        "options": {
            "enable": true,
            "target": {
                "installable": ""
            }
        },
        "diagnostic": {
            "suppress": []
        }
    })
}

pub(super) fn yaml_init_options() -> Value {
    json!({
        "yaml": {
            "validate": true,
            "hover": true,
            "completion": true,
            "format": {
                "enable": true,
                "singleQuote": false,
                "bracketSpacing": true,
                "proseWrap": "preserve"
            },
            "schemaStore": {
                "enable": true,
                "url": "https://www.schemastore.org/api/json/catalog.json"
            },
            "schemas": {},
            "customTags": [],
            "maxItemsComputed": 5000
        }
    })
}

pub(super) fn terraform_init_options() -> Value {
    json!({
        "terraform": {
            "indexing": {
                "ignorePaths": [],
                "ignoreDirectoryNames": [".terraform", ".git"]
            },
            "validation": {
                "enableEnhancedValidation": true
            },
            "experimentalFeatures": {
                "validateOnSave": true,
                "prefillRequiredFields": true
            }
        },
        "terraform-ls": {
            "rootModulePaths": []
        }
    })
}

pub(super) fn zig_init_options() -> Value {
    json!({
        // Core settings
        "enable_snippets": true,
        "enable_argument_placeholders": true,
        "enable_ast_check_diagnostics": true,
        "enable_autofix": true,
        "enable_import_cycle_warnings": true,
        "enable_semantic_tokens": true,
        "semantic_tokens": "full",

        // Build settings
        "enable_build_on_save": true,
        "build_on_save_args": ["build"],
        "prefer_ast_check_as_child_process": true,

        // Inlay hints
        "enable_inlay_hints": false,
        "inlay_hints_show_variable_type_hints": true,
        "inlay_hints_show_struct_literal_field_types": true,
        "inlay_hints_show_parameter_name_hints": true,
        "inlay_hints_show_builtin": true,
        "inlay_hints_exclude_single_argument": true,
        "inlay_hints_hide_redundant_param_names": false,
        "inlay_hints_hide_redundant_param_names_last_token": false,

        // Other settings
        "completion_label_details": true,
        "warn_style": false,
        "highlight_global_var_declarations": false,
        "highlight_global_builtin": true,
        "dangerous_comptime_ast_checks": false,
        "skip_std_references": false,
        "prefer_build_runner_to_build_file": true
    })
}

pub(super) fn clojure_init_options() -> Value {
    json!({
        "dependency-scheme": "jar",
        "text-document-sync-kind": "incremental",
        "source-paths": ["src", "test"],
        "source-aliases": [],
        "hover": {
            "arity-on-same-line?": true,
            "hide-file-location?": false,
            "clojuredocs": true
        },
        "completion": {
            "additional-edits-warning-text": null
        },
        "semantic-tokens?": true,
        "lint": {
            "forward-diagnostics": true
        },
        "cljfmt-raw": null,
        "java": {
            "home-path": null,
            "decompile-jar-as-project?": true
        }
    })
}

pub(super) fn elm_init_options() -> Value {
    json!({
        "elmPath": "elm",
        "elmFormatPath": "elm-format",
        "elmTestPath": "elm-test",
        "elmReviewPath": "elm-review",
        "skipInstallPackageConfirmation": true,
        "disableElmLSDiagnostics": false,
        "onlyUpdateDiagnosticsOnSave": false,
        "elmReviewDiagnostics": "warning"
    })
}

pub(super) fn erlang_init_options() -> Value {
    json!({
        "codePath": [],
        "includeFileExt": ["hrl", "erl"],
        "excludeFileExt": ["beam"],
        "excludePaths": ["_build", "deps", "ebin", ".rebar3", "logs", "_checkouts"],
        "diagnostics": {
            "enabled": true,
            "enabledOtpDiagnostics": true
        },
        "inlayHints": {
            "enabled": false
        },
        "lenses": {
            "enabled": true
        }
    })
}

pub(super) fn swift_init_options() -> Value {
    json!({
        "backgroundIndexing": true,
        "backgroundIndexingDeferred": false,
        "compilationDatabaseBuildDirectory": null,
        "completionMaxResults": 200,
        "fallbackBuildSystem": "auto",
        "index": {
            "prefixMappings": {}
        },
        "logging": {
            "level": "warning"
        },
        "sourcekitdOptions": [],
        "swiftSDK": null,
        "swiftCompilerFlags": []
    })
}

pub(super) fn ocaml_init_options() -> Value {
    json!({
        "codelens": {
            "enable": true
        },
        "extendedHover": {
            "enable": true
        },
        "dune": {
            "autoFmt": false
        },
        "syntaxDocumentation": {
            "enable": true
        },
        "inlayHints": {
            "enable": false
        }
    })
}

pub(super) fn bash_init_options() -> Value {
    json!({
        "locale": "en"
    })
}

pub(super) fn vue_init_options() -> Value {
    json!({
        "vue": {
            "hybridMode": true
        },
        "typescript": {
            "tsdk": null
        },
        "completion": {
            "autoInsertDotValue": true,
            "autoInsertParentheses": true
        },
        "inlayHints": {
            "missingProps": false,
            "inlineHandlerLeading": false
        },
        "codeActions": {
            "enabled": true,
            "savingTimeLimit": 1000
        },
        "format": {
            "enable": true
        }
    })
}
