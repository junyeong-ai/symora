//! Language-specific LSP initialization options

use serde_json::{Value, json};
use std::path::Path;

use crate::models::lsp::path_to_uri;
use crate::models::symbol::Language;

pub fn get_initialization_options(language: Language, root_path: &Path) -> Option<Value> {
    match language {
        Language::Kotlin => Some(kotlin_init_options(root_path)),
        Language::TypeScript | Language::JavaScript => Some(typescript_init_options()),
        Language::Python => Some(python_init_options()),
        Language::Rust => Some(rust_init_options()),
        Language::Java => Some(java_init_options(root_path)),
        Language::Go => Some(go_init_options()),
        Language::CSharp => Some(csharp_init_options()),
        // clangd doesn't use initializationOptions - configured via command line args
        Language::Cpp => None,
        Language::Ruby => Some(ruby_init_options()),
        Language::PHP => Some(php_init_options()),
        Language::Lua => Some(lua_init_options()),
        Language::Elixir => Some(elixir_init_options()),
        Language::Scala => Some(scala_init_options()),
        Language::Haskell => Some(haskell_init_options()),
        Language::Dart => Some(dart_init_options()),
        Language::Nix => Some(nix_init_options()),
        Language::Yaml => Some(yaml_init_options()),
        Language::Terraform => Some(terraform_init_options()),
        Language::Zig => Some(zig_init_options()),
        Language::Clojure => Some(clojure_init_options()),
        Language::Elm => Some(elm_init_options()),
        Language::Erlang => Some(erlang_init_options()),
        Language::FSharp => Some(fsharp_init_options(root_path)),
        Language::Swift => Some(swift_init_options()),
        Language::OCaml => Some(ocaml_init_options()),
        Language::Vue => Some(vue_init_options()),
        Language::Bash => Some(bash_init_options()),
        _ => None,
    }
}

fn kotlin_init_options(root_path: &Path) -> Value {
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

fn typescript_init_options() -> Value {
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

fn python_init_options() -> Value {
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

fn rust_init_options() -> Value {
    json!({
        "cargo": {
            "autoreload": true,
            "buildScripts": {
                "enable": true,
                "invocationLocation": "workspace",
                "invocationStrategy": "per_workspace",
                "overrideCommand": null,
                "rebuildOnSave": true,
                "useRustcWrapper": true
            },
            "cfgs": [],
            "extraArgs": [],
            "extraEnv": {},
            "features": "all",
            "noDefaultFeatures": false,
            "sysroot": "discover",
            "sysrootSrc": null,
            "target": null,
            "unsetTest": ["core"]
        },
        "check": {
            "allTargets": true,
            "command": "clippy",
            "extraArgs": ["--", "-W", "clippy::all"],
            "extraEnv": {},
            "features": null,
            "ignore": [],
            "invocationLocation": "workspace",
            "invocationStrategy": "per_workspace",
            "noDefaultFeatures": null,
            "overrideCommand": null,
            "targets": null,
            "workspace": true
        },
        "procMacro": {
            "enable": true,
            "attributes": {
                "enable": true
            },
            "ignored": {}
        },
        "diagnostics": {
            "enable": true,
            "experimental": {
                "enable": true
            },
            "remapPrefix": {},
            "styleLints": {
                "enable": true
            }
        },
        "inlayHints": {
            "enable": false,
            "bindingModeHints": {
                "enable": false
            },
            "closingBraceHints": {
                "enable": false,
                "minLines": 25
            },
            "closureCaptureHints": {
                "enable": false
            },
            "closureReturnTypeHints": {
                "enable": "never"
            },
            "closureStyle": "impl_fn",
            "discriminantHints": {
                "enable": "never"
            },
            "expressionAdjustmentHints": {
                "enable": "never",
                "hideOutsideUnsafe": false,
                "mode": "prefix"
            },
            "lifetimeElisionHints": {
                "enable": "never",
                "useParameterNames": false
            },
            "maxLength": 25,
            "parameterHints": {
                "enable": false
            },
            "reborrowHints": {
                "enable": "never"
            },
            "renderColons": true,
            "typeHints": {
                "enable": false,
                "hideClosureInitialization": false,
                "hideNamedConstructor": false
            }
        },
        "completion": {
            "autoimport": {
                "enable": true
            },
            "autoself": {
                "enable": true
            },
            "callable": {
                "snippets": "fill_arguments"
            },
            "fullFunctionSignatures": {
                "enable": false
            },
            "limit": null,
            "postfix": {
                "enable": true
            },
            "privateEditable": {
                "enable": true
            },
            "snippets": {
                "custom": {
                    "Arc::new": {
                        "postfix": "arc",
                        "body": "Arc::new(${receiver})",
                        "requires": "std::sync::Arc",
                        "description": "Put the expression into an `Arc`",
                        "scope": "expr"
                    },
                    "Rc::new": {
                        "postfix": "rc",
                        "body": "Rc::new(${receiver})",
                        "requires": "std::rc::Rc",
                        "description": "Put the expression into an `Rc`",
                        "scope": "expr"
                    },
                    "Box::pin": {
                        "postfix": "pinbox",
                        "body": "Box::pin(${receiver})",
                        "requires": "std::boxed::Box",
                        "description": "Put the expression into a pinned `Box`",
                        "scope": "expr"
                    },
                    "Ok": {
                        "postfix": "ok",
                        "body": "Ok(${receiver})",
                        "description": "Wrap the expression in a `Result::Ok`",
                        "scope": "expr"
                    },
                    "Err": {
                        "postfix": "err",
                        "body": "Err(${receiver})",
                        "description": "Wrap the expression in a `Result::Err`",
                        "scope": "expr"
                    },
                    "Some": {
                        "postfix": "some",
                        "body": "Some(${receiver})",
                        "description": "Wrap the expression in an `Option::Some`",
                        "scope": "expr"
                    }
                }
            },
            "termSearch": {
                "enable": false
            }
        },
        "hover": {
            "actions": {
                "enable": true,
                "debug": {
                    "enable": true
                },
                "gotoTypeDef": {
                    "enable": true
                },
                "implementations": {
                    "enable": true
                },
                "references": {
                    "enable": true
                },
                "run": {
                    "enable": true
                }
            },
            "documentation": {
                "enable": true,
                "keywords": {
                    "enable": true
                }
            },
            "links": {
                "enable": true
            },
            "memoryLayout": {
                "enable": true,
                "alignment": "hexadecimal",
                "niches": false,
                "offset": "hexadecimal",
                "size": "both"
            },
            "show": {
                "enumVariants": 5,
                "fields": 5,
                "traitAssocItems": null
            }
        },
        "imports": {
            "granularity": {
                "enforce": false,
                "group": "crate"
            },
            "group": {
                "enable": true
            },
            "merge": {
                "glob": true
            },
            "preferNoStd": false,
            "preferPrelude": false,
            "prefix": "plain"
        },
        "semanticHighlighting": {
            "doc": {
                "comment": {
                    "inject": {
                        "enable": true
                    }
                }
            },
            "nonStandardTokens": true,
            "operator": {
                "enable": true,
                "specialization": {
                    "enable": true
                }
            },
            "punctuation": {
                "enable": true,
                "separate": {
                    "macro": {
                        "bang": true
                    }
                },
                "specialization": {
                    "enable": true
                }
            },
            "strings": {
                "enable": true
            }
        },
        "lens": {
            "enable": true,
            "forceCustomCommands": true,
            "implementations": {
                "enable": true
            },
            "location": "above_name",
            "references": {
                "adt": {
                    "enable": false
                },
                "enumVariant": {
                    "enable": false
                },
                "method": {
                    "enable": false
                },
                "trait": {
                    "enable": false
                }
            },
            "run": {
                "enable": true
            }
        },
        "workspace": {
            "symbol": {
                "search": {
                    "kind": "only_types",
                    "limit": 128,
                    "scope": "workspace"
                }
            }
        }
    })
}

fn java_init_options(root_path: &Path) -> Value {
    let root_uri = path_to_uri(root_path);
    let java_home = detect_java_home();
    let gradle_home = std::env::var("GRADLE_HOME").ok();
    let gradle_user_home = std::env::var("GRADLE_USER_HOME")
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".gradle").to_string_lossy().to_string()));
    let maven_settings = detect_maven_settings();
    let maven_user_settings = dirs::home_dir()
        .map(|h| h.join(".m2").join("settings.xml"))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string());

    json!({
        "bundles": [],
        "workspaceFolders": [root_uri],
        "settings": {
            "java": {
                "home": java_home,
                "jdt": {
                    "ls": {
                        "lombokSupport": { "enabled": true },
                        "protobufSupport": { "enabled": true },
                        "androidSupport": { "enabled": true },
                        "vmargs": "-XX:+UseParallelGC -XX:GCTimeRatio=4 -XX:AdaptiveSizePolicyWeight=90 -Xmx2G -Xms100m"
                    }
                },
                "configuration": {
                    "updateBuildConfiguration": "automatic",
                    "workspaceCacheLimit": 90,
                    "runtimes": []
                },
                "import": {
                    "gradle": {
                        "enabled": true,
                        "wrapper": { "enabled": true },
                        "offline": { "enabled": false },
                        "annotationProcessing": { "enabled": true },
                        "arguments": null,
                        "home": gradle_home,
                        "java": { "home": null },
                        "jvmArguments": null,
                        "user": { "home": gradle_user_home },
                        "version": null
                    },
                    "maven": {
                        "enabled": true,
                        "downloadSources": true,
                        "updateSnapshots": false,
                        "notCoveredPluginExecutionSeverity": "warning",
                        "defaultMojoExecutionAction": "ignore",
                        "disableTestClasspathFlag": false,
                        "globalSettings": maven_settings,
                        "userSettings": maven_user_settings
                    },
                    "exclusions": [
                        "**/node_modules/**",
                        "**/.metadata/**",
                        "**/archetype-resources/**",
                        "**/META-INF/maven/**",
                        "**/build/**",
                        "**/target/**",
                        "**/bin/**",
                        "**/out/**",
                        "**/generated/**",
                        "**/generated-sources/**",
                        "**/generated-test-sources/**",
                        "**/*Proto.java",
                        "**/*Grpc.java"
                    ],
                    "generatesMetadataFilesAtProjectRoot": false
                },
                "format": {
                    "enabled": true,
                    "insertSpaces": true,
                    "tabSize": 4,
                    "onType": { "enabled": true }
                },
                "compile": {
                    "nullAnalysis": {
                        "nonnull": [
                            "javax.annotation.Nonnull",
                            "org.eclipse.jdt.annotation.NonNull",
                            "org.springframework.lang.NonNull",
                            "lombok.NonNull",
                            "org.jetbrains.annotations.NotNull"
                        ],
                        "nullable": [
                            "javax.annotation.Nullable",
                            "org.eclipse.jdt.annotation.Nullable",
                            "org.springframework.lang.Nullable",
                            "org.jetbrains.annotations.Nullable"
                        ],
                        "mode": "automatic"
                    }
                },
                "inlayHints": {
                    "parameterNames": { "enabled": "literals" }
                },
                "references": {
                    "includeAccessors": true,
                    "includeDecompiledSources": true
                },
                "signatureHelp": { "enabled": true },
                "selectionRange": { "enabled": true },
                "completion": {
                    "enabled": true,
                    "favoriteStaticMembers": [
                        "org.junit.Assert.*",
                        "org.junit.jupiter.api.Assertions.*",
                        "org.mockito.Mockito.*",
                        "org.mockito.ArgumentMatchers.*",
                        "org.assertj.core.api.Assertions.*"
                    ],
                    "filteredTypes": [
                        "com.sun.*",
                        "io.micrometer.shaded.*",
                        "java.awt.*",
                        "jdk.*",
                        "sun.*"
                    ],
                    "guessMethodArguments": true,
                    "importOrder": ["java", "javax", "org", "com", ""]
                },
                "sources": {
                    "organizeImports": {
                        "starThreshold": 99,
                        "staticStarThreshold": 99
                    }
                },
                "cleanup": {
                    "actionsOnSave": []
                }
            }
        }
    })
}

fn go_init_options() -> Value {
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

fn csharp_init_options() -> Value {
    json!({
        "RoslynExtensionsOptions": {
            "enableAnalyzersSupport": true,
            "enableImportCompletion": true,
            "enableDecompilationSupport": true,
            "enableAsyncCompletion": false,
            "documentAnalysisTimeoutMs": 30000,
            "diagnosticWorkersThreadCount": 8,
            "analyzeOpenDocumentsOnly": true,
            "inlayHintsOptions": {
                "enableForParameters": false,
                "forLiteralParameters": false,
                "forObjectCreationParameters": false,
                "enableForTypes": false,
                "forImplicitVariableTypes": false,
                "forLambdaParameterTypes": false,
                "forImplicitObjectCreation": false
            },
            "locationPaths": null
        },
        "FormattingOptions": {
            "enableEditorConfigSupport": true,
            "organizeImports": false,
            "newLine": "\n",
            "useTabs": false,
            "tabSize": 4,
            "indentationSize": 4
        },
        "FileOptions": {
            "systemExcludeSearchPatterns": [
                "**/node_modules/**/*",
                "**/bin/**/*",
                "**/obj/**/*",
                "**/.git/**/*"
            ],
            "excludeSearchPatterns": []
        },
        "RenameOptions": {
            "renameInComments": false,
            "renameInStrings": false,
            "renameOverloads": true
        },
        "ImplementTypeOptions": {
            "insertionBehavior": "WithOtherMembersOfTheSameKind",
            "propertyGenerationBehavior": "PreferAutoProperties"
        },
        "DotNetCliOptions": {
            "locationPaths": null
        },
        "Plugins": {
            "locationPaths": null
        }
    })
}

fn ruby_init_options() -> Value {
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

fn php_init_options() -> Value {
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

fn lua_init_options() -> Value {
    json!({
        "runtime": {
            "version": "LuaJIT",
            "path": ["?.lua", "?/init.lua"]
        },
        "diagnostics": {
            "enable": true,
            "globals": ["vim", "describe", "it", "before_each", "after_each", "setup", "teardown"],
            "disable": [],
            "severity": {
                "undefined-global": "Error",
                "lowercase-global": "Warning"
            }
        },
        "workspace": {
            "checkThirdParty": false,
            "library": [],
            "ignoreDir": [".git", "node_modules", "build", "dist", ".luarocks", "lua_modules", ".cache"],
            "maxPreload": 1000,
            "preloadFileSize": 1048576
        },
        "completion": {
            "enable": true,
            "callSnippet": "Both"
        },
        "hint": {
            "enable": false,
            "paramType": false,
            "setType": false
        },
        "type": {
            "castNumberToInteger": true,
            "weakUnionCheck": true
        },
        "telemetry": {
            "enable": false
        }
    })
}

fn elixir_init_options() -> Value {
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

fn scala_init_options() -> Value {
    json!({
        "bloopSbtAlreadyInstalled": false,
        "superMethodLensesEnabled": false,
        "showImplicitArguments": false,
        "showImplicitConversionsAndClasses": false,
        "showInferredType": false,
        "excludedPackages": [],
        "decorationProvider": false,
        "inlineDecorationProvider": false,
        "statusBarProvider": "off",
        "treeViewProvider": false,
        "debuggingProvider": true,
        "isHttpEnabled": true,
        "isExitOnShutdown": true,
        "globSyntax": "uri",
        "icons": "unicode",
        "inputBoxProvider": false,
        "isVirtualDocumentSupported": false,
        "openFilesOnRenameProvider": false,
        "quickPickProvider": false,
        "renameFileThreshold": 200,
        "testExplorerProvider": false,
        "openNewWindowProvider": false,
        "copyWorksheetOutputProvider": false,
        "doctorVisibilityProvider": false,
        "compilerOptions": {
            "completionCommand": null,
            "isCompletionItemDetailEnabled": true,
            "isCompletionItemDocumentationEnabled": true,
            "isCompletionItemResolve": true,
            "isHoverDocumentationEnabled": true,
            "isSignatureHelpDocumentationEnabled": true,
            "overrideDefFormat": "ascii",
            "snippetAutoIndent": false
        }
    })
}

fn haskell_init_options() -> Value {
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

fn dart_init_options() -> Value {
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

fn nix_init_options() -> Value {
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

fn yaml_init_options() -> Value {
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

fn terraform_init_options() -> Value {
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

fn zig_init_options() -> Value {
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

fn clojure_init_options() -> Value {
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

fn elm_init_options() -> Value {
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

fn erlang_init_options() -> Value {
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

fn fsharp_init_options(root_path: &Path) -> Value {
    let root_uri = path_to_uri(root_path);
    json!({
        "automaticWorkspaceInit": true,
        "workspacePath": root_uri,
        "workspaceModePeekDeepLevel": 2,
        "abstractClassStubGeneration": true,
        "abstractClassStubGenerationObjectIdentifier": "this",
        "abstractClassStubGenerationMethodBody": "failwith \"Not Implemented\"",
        "addPrivateAccessModifier": false,
        "unusedOpensAnalyzer": true,
        "unusedDeclarationsAnalyzer": true,
        "simplifyNameAnalyzer": true,
        "resolveNamespaces": true,
        "enableReferenceCodeLens": true,
        "dotNetRoot": null,
        "fsiExtraParameters": [],
        "linter": true,
        "indentationSize": 4,
        "interfaceStubGeneration": true,
        "pipelineHints": {
            "enabled": true
        },
        "fsac": {
            "cachedTypeCheckCount": 200,
            "conserveMemory": true,
            "silencedLogs": [],
            "analyzersPath": [],
            "sourceTextImplementation": "NamedText"
        },
        "codeLenses": {
            "signature": {
                "enabled": true
            },
            "references": {
                "enabled": true
            }
        },
        "inlayHints": {
            "enabled": false,
            "typeAnnotations": false,
            "parameterNames": false,
            "disableLongTooltip": true
        },
        "debug": {
            "dontCheckRelatedFiles": false,
            "checkFileDebouncerTimeout": 250
        }
    })
}

fn swift_init_options() -> Value {
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

fn ocaml_init_options() -> Value {
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

fn bash_init_options() -> Value {
    json!({
        "locale": "en"
    })
}

fn vue_init_options() -> Value {
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

fn detect_java_home() -> Option<String> {
    std::env::var("JAVA_HOME").ok().or_else(|| {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("/usr/libexec/java_home")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    })
}

fn detect_maven_settings() -> Option<String> {
    std::env::var("M2_HOME")
        .ok()
        .map(|h| {
            std::path::PathBuf::from(h)
                .join("conf")
                .join("settings.xml")
        })
        .or_else(|| {
            std::env::var("MAVEN_HOME").ok().map(|h| {
                std::path::PathBuf::from(h)
                    .join("conf")
                    .join("settings.xml")
            })
        })
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_kotlin_init_options() {
        let root = PathBuf::from("/test/project");
        let opts = kotlin_init_options(&root);

        assert!(opts.get("indexing").is_some());
        assert_eq!(opts["diagnostics"]["level"], 4);
        assert_eq!(opts["indexing"]["enabled"], true);
    }

    #[test]
    fn test_typescript_init_options() {
        let opts = typescript_init_options();
        assert_eq!(opts["hostInfo"], "symora");
    }

    #[test]
    fn test_lua_init_options() {
        let opts = lua_init_options();
        assert_eq!(opts["runtime"]["version"], "LuaJIT");
        assert_eq!(opts["workspace"]["checkThirdParty"], false);
    }

    #[test]
    fn test_elixir_init_options() {
        let opts = elixir_init_options();
        assert_eq!(opts["mixEnv"], "test");
        assert_eq!(opts["dialyzerEnabled"], true);
    }

    #[test]
    fn test_scala_init_options() {
        let opts = scala_init_options();
        assert_eq!(opts["bloopSbtAlreadyInstalled"], false);
        assert_eq!(opts["statusBarProvider"], "off");
    }

    #[test]
    fn test_haskell_init_options() {
        let opts = haskell_init_options();
        assert_eq!(opts["haskell"]["checkProject"], false);
        assert_eq!(opts["haskell"]["formattingProvider"], "ormolu");
    }

    #[test]
    fn test_get_initialization_options() {
        let root = PathBuf::from("/test");

        let languages_with_options = [
            Language::Kotlin,
            Language::TypeScript,
            Language::JavaScript,
            Language::Python,
            Language::Rust,
            Language::Java,
            Language::Go,
            Language::CSharp,
            // Note: Cpp (clangd) doesn't use initializationOptions
            Language::Ruby,
            Language::PHP,
            Language::Lua,
            Language::Elixir,
            Language::Scala,
            Language::Haskell,
            Language::Dart,
            Language::Nix,
            Language::Yaml,
            Language::Terraform,
            Language::Zig,
            Language::Clojure,
            Language::Elm,
            Language::Erlang,
            Language::FSharp,
            Language::Swift,
            Language::OCaml,
            Language::Vue,
        ];

        for lang in languages_with_options {
            assert!(
                get_initialization_options(lang, &root).is_some(),
                "Expected init options for {:?}",
                lang
            );
        }

        assert!(get_initialization_options(Language::Unknown, &root).is_none());

        // Verify Cpp returns None (clangd uses CLI args, not initializationOptions)
        assert!(
            get_initialization_options(Language::Cpp, &root).is_none(),
            "Cpp (clangd) should not have init options"
        );
    }

    #[test]
    fn test_dart_init_options() {
        let opts = dart_init_options();
        assert_eq!(opts["closingLabels"], true);
        assert_eq!(opts["documentation"], "full");
    }

    #[test]
    fn test_nix_init_options() {
        let opts = nix_init_options();
        assert!(opts["nixpkgs"]["expr"].is_string());
    }

    #[test]
    fn test_yaml_init_options() {
        let opts = yaml_init_options();
        assert_eq!(opts["yaml"]["validate"], true);
        assert_eq!(opts["yaml"]["schemaStore"]["enable"], true);
    }

    #[test]
    fn test_terraform_init_options() {
        let opts = terraform_init_options();
        assert_eq!(
            opts["terraform"]["validation"]["enableEnhancedValidation"],
            true
        );
    }

    #[test]
    fn test_zig_init_options() {
        let opts = zig_init_options();
        assert_eq!(opts["enable_semantic_tokens"], true);
        assert_eq!(opts["enable_autofix"], true);
    }

    #[test]
    fn test_fsharp_init_options() {
        let root = PathBuf::from("/test/project");
        let opts = fsharp_init_options(&root);
        assert_eq!(opts["automaticWorkspaceInit"], true);
        assert_eq!(opts["linter"], true);
    }
}
