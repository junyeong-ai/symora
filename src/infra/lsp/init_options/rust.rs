use serde_json::{Value, json};
pub(super) fn rust_init_options() -> Value {
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
