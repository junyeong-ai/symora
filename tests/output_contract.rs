//! Snapshot tests for the public JSON output contract.
//!
//! These tests pin the wire format of every response type the CLI emits.
//! They run without any LSP, daemon, or filesystem state, so a snapshot
//! diff means the contract changed — not that the environment shifted.
//!
//! Workflow:
//!   - Make the change.
//!   - `cargo test --test output_contract` — failures show before/after.
//!   - If the new output is intentional, `cargo insta review` to accept.
//!
//! Add a new snapshot by writing another `assert_json_snapshot!` block.

use std::path::PathBuf;

use insta::assert_json_snapshot;
use serde_json::json;
use symora::cli::blast_radius::{BlastRadius, DepthBucket, RiskLevel};
use symora::cli::commands::diagnostics::{DiagnosticsOutput, EnhancedDiagnostic};
use symora::cli::errors::{ErrorCode, OutputError};
use symora::cli::response::{
    ActionOutput, AffectedFileOutput, ApplyActionOutput, CallHierarchyOutput, CoverageGap,
    DefinitionOutput, DiagnosticOutput, EditOutput, FileChangeOutput, HoverOutput, ImpactOutput,
    LineRange, LocationOutput, ParameterOutput, RefOutput, Section, ServerStatusOutput,
    SignatureHelpOutput, SignatureItemOutput, SymbolOutput, TargetOutput, TestCoverageOutput,
    TestOutput, TypeInfoOutput, fit_to_char_budget,
};
use symora::models::diagnostic::DiagnosticsStatus;
use symora::models::lsp::{CallHierarchyItem, IndexingDegradation, TypeHierarchyItem};
use symora::models::symbol::{Language, Location, Symbol, SymbolKind};

fn root() -> PathBuf {
    PathBuf::from("/repo")
}

fn sample_location(line: u32, column: u32) -> LocationOutput {
    LocationOutput {
        file: "src/main.rs".to_string(),
        line,
        column,
        snippet: None,
        degraded_column: None,
    }
}

fn sample_symbol() -> Symbol {
    let mut sym = Symbol::new(
        "process".to_string(),
        SymbolKind::Function,
        Location::full(root().join("src/main.rs"), 12, 4, 12, 4, 20, 1),
    )
    .with_container("Handler".to_string())
    .with_body("fn process(&self) {}".to_string());
    sym.compute_paths(Some("Handler"));
    sym
}

#[test]
fn section_empty() {
    let section: Section<i32> = Section::new(vec![]);
    assert_json_snapshot!(section, @r###"
    {
      "count": 0,
      "showing": 0,
      "items": []
    }
    "###);
}

#[test]
fn section_with_items_and_truncation() {
    let section = Section::with_total(vec![1u32, 2, 3], 10);
    assert_json_snapshot!(section, @r###"
    {
      "count": 10,
      "showing": 3,
      "items": [
        1,
        2,
        3
      ],
      "truncated": true
    }
    "###);
}

#[test]
fn section_with_hints_and_next_commands() {
    let section = Section::with_total(vec![1u32], 4)
        .with_hints(vec!["narrow the query".to_string()])
        .with_next_commands(vec!["symora map file src/a.rs".to_string()]);
    assert_json_snapshot!(section, @r###"
    {
      "count": 4,
      "showing": 1,
      "items": [
        1
      ],
      "truncated": true,
      "hints": [
        "narrow the query"
      ],
      "next_commands": [
        "symora map file src/a.rs"
      ]
    }
    "###);
}

/// Pins the post-fit shape and the disclosure wording of the
/// `output.max_response_chars` size ceiling: items dropped whole from the
/// tail, `showing` updated, `truncated` set, `count` untouched, one hint
/// naming the config key — and no new envelope keys.
#[test]
fn section_fitted_to_char_budget() {
    let items: Vec<String> = (1..=10)
        .map(|i| format!("src/module_{i:02}.rs:1: reference"))
        .collect();
    let mut value = serde_json::to_value(Section::new(items)).unwrap();

    let fitted = fit_to_char_budget(&mut value, 300, &|v: &serde_json::Value| {
        serde_json::to_string(v)
            .map(|s| s.chars().count())
            .unwrap_or(usize::MAX)
    });

    assert!(fitted);
    assert_json_snapshot!(value);
}

/// The output layer injects `config_errors` BEFORE the size fit (so the ceiling
/// accounts for the disclosure), relying on the fitter trimming only whole
/// `Section` items — never the top-level `config_errors` array. This pins that
/// safety property: a load failure rides along even when the budget forces
/// items to be dropped.
#[test]
fn fitter_preserves_config_errors_while_trimming_section_items() {
    let items: Vec<String> = (1..=10)
        .map(|i| format!("src/module_{i:02}.rs:1: reference"))
        .collect();
    let mut value = serde_json::to_value(Section::new(items)).unwrap();
    value.as_object_mut().unwrap().insert(
        "config_errors".to_string(),
        json!(["failed to load .symora/config.toml: expected a value"]),
    );

    let fitted = fit_to_char_budget(&mut value, 200, &|v: &serde_json::Value| {
        serde_json::to_string(v)
            .map(|s| s.chars().count())
            .unwrap_or(usize::MAX)
    });

    assert!(fitted);
    assert!(
        value.get("config_errors").is_some(),
        "config_errors must survive the size fit — it is never dropped to fit the budget"
    );
    assert!(
        value["showing"].as_u64().unwrap() < 10,
        "section items are trimmed to stay under budget"
    );
}

/// Pins the disclosure field of `context --with-bodies`: present only on
/// sections where body attachment ran, equal to the number of items
/// carrying a `body`.
#[test]
fn section_with_bodies_included() {
    let section = Section::new(vec![1u32]).with_bodies_included(Some(1));
    assert_json_snapshot!(section, @r###"
    {
      "count": 1,
      "showing": 1,
      "items": [
        1
      ],
      "bodies_included": 1
    }
    "###);
}

#[test]
fn section_with_structured_error() {
    let section: Section<i32> =
        Section::error(OutputError::not_found("symbol not found").with_hint("try a broader query"));
    assert_json_snapshot!(section, @r###"
    {
      "count": 0,
      "showing": 0,
      "items": [],
      "error": {
        "code": "not_found",
        "message": "symbol not found",
        "hint": "try a broader query"
      }
    }
    "###);
}

#[test]
fn output_error_all_codes_serialize_in_snake_case() {
    let codes = [
        ErrorCode::NotFound,
        ErrorCode::Unsupported,
        ErrorCode::Timeout,
        ErrorCode::InvalidArgument,
        ErrorCode::Internal,
        ErrorCode::LspUnavailable,
        ErrorCode::LanguageNotConfigured,
        ErrorCode::ServerNotInstalled,
        ErrorCode::Cancelled,
        ErrorCode::ParseError,
        ErrorCode::StoreNotInitialized,
        ErrorCode::AlreadyExists,
        ErrorCode::Conflict,
        ErrorCode::PreconditionFailed,
        ErrorCode::FileTooLarge,
        ErrorCode::Io,
    ];
    let payload: Vec<_> = codes
        .iter()
        .copied()
        .map(|c| OutputError::new(c, "x"))
        .collect();
    assert_json_snapshot!(payload);
}

#[test]
fn location_output_skips_none_snippet() {
    assert_json_snapshot!(sample_location(10, 5), @r###"
    {
      "file": "src/main.rs",
      "line": 10,
      "column": 5
    }
    "###);
}

#[test]
fn location_output_includes_snippet_when_present() {
    let loc = LocationOutput {
        snippet: Some("let x = 1;".to_string()),
        ..sample_location(10, 5)
    };
    assert_json_snapshot!(loc, @r###"
    {
      "file": "src/main.rs",
      "line": 10,
      "column": 5,
      "snippet": "let x = 1;"
    }
    "###);
}

#[test]
fn section_discloses_coverage_gaps() {
    // A search for an explicitly requested but unindexed --lang discloses a
    // machine-branchable gap, so an empty result reads "not indexed here, try
    // ast/content" rather than "no such symbol". Omitted when there are none.
    let section: Section<LocationOutput> =
        Section::new(vec![]).with_coverage_gaps(vec![CoverageGap {
            language: "lua".to_string(),
            reason: "not_indexed".to_string(),
        }]);
    assert_json_snapshot!(section, @r###"
    {
      "count": 0,
      "showing": 0,
      "items": [],
      "coverage_gaps": [
        {
          "language": "lua",
          "reason": "not_indexed"
        }
      ]
    }
    "###);
}

#[test]
fn location_output_discloses_a_degraded_column() {
    // A degraded column (decoded from an unreadable line) surfaces the flag so
    // an agent can tell a wire-offset guess from a transcoded value; it is
    // omitted in the common case (the snippet test above carries no flag).
    let loc = LocationOutput {
        degraded_column: Some(true),
        ..sample_location(10, 5)
    };
    assert_json_snapshot!(loc, @r###"
    {
      "file": "src/main.rs",
      "line": 10,
      "column": 5,
      "degraded_column": true
    }
    "###);
}

#[test]
fn symbol_output_full() {
    let sym = sample_symbol();
    let out = SymbolOutput::from_symbol(&sym, &root())
        .with_signature(Some("fn process(&self) -> ()".to_string()));
    assert_json_snapshot!(out);
}

#[test]
fn symbol_output_without_body_or_children() {
    let sym = sample_symbol();
    let out = SymbolOutput::from_symbol(&sym, &root())
        .without_body()
        .without_children();
    assert_json_snapshot!(out);
}

#[test]
fn definition_output_with_location_only() {
    let out = DefinitionOutput {
        definition: Some(sample_location(20, 8)),
        message: None,
    };
    assert_json_snapshot!(out);
}

#[test]
fn definition_output_with_message_only() {
    let out = DefinitionOutput {
        definition: None,
        message: Some("no definition found".to_string()),
    };
    assert_json_snapshot!(out);
}

#[test]
fn hover_output_full() {
    let out = HoverOutput {
        content: Some("fn process(&self)".to_string()),
        range: Some(sample_location(12, 4)),
        message: None,
    };
    assert_json_snapshot!(out);
}

#[test]
fn diagnostic_output_full() {
    let out = DiagnosticOutput {
        severity: "error".to_string(),
        message: "type mismatch".to_string(),
        line: 30,
        column: 8,
        end_line: 30,
        end_column: 16,
        code: Some("E0308".to_string()),
        source: Some("rust-analyzer".to_string()),
        tags: vec!["unnecessary".to_string()],
    };
    assert_json_snapshot!(out);
}

#[test]
fn diagnostics_output_flattened_section_with_status() {
    let out = DiagnosticsOutput {
        file: "src/main.rs".to_string(),
        status: DiagnosticsStatus::Unconfirmed,
        diagnostics: Section::new(vec![EnhancedDiagnostic {
            base: DiagnosticOutput {
                severity: "error".to_string(),
                message: "type mismatch".to_string(),
                line: 30,
                column: 8,
                end_line: 30,
                end_column: 16,
                code: Some("E0308".to_string()),
                source: Some("rust-analyzer".to_string()),
                tags: vec![],
            },
            context: vec![],
            suggestions: vec![],
            suggestions_status: None,
        }]),
    };
    assert_json_snapshot!(out);
}

#[test]
fn call_hierarchy_output_with_call_site() {
    let item = CallHierarchyItem {
        name: "process".to_string(),
        kind: SymbolKind::Function,
        location: Location::point(root().join("src/main.rs"), 12, 4),
        call_site: Some(Location::point(root().join("src/api.rs"), 50, 12)),
    };
    let out = CallHierarchyOutput::from_item(&item, &root());
    assert_json_snapshot!(out);
}

/// `body` appears on a callee item only when `context --with-bodies`
/// admitted it — every other producer leaves it `None` (omitted).
#[test]
fn call_hierarchy_output_with_body() {
    let out = CallHierarchyOutput {
        name: "callee".to_string(),
        location: sample_location(12, 4),
        call_site: None,
        body: Some("fn callee() {}".to_string()),
    };
    assert_json_snapshot!(out, @r###"
    {
      "name": "callee",
      "location": {
        "file": "src/main.rs",
        "line": 12,
        "column": 4
      },
      "body": "fn callee() {}"
    }
    "###);
}

#[test]
fn target_output_from_symbol() {
    let sym = sample_symbol();
    let out = TargetOutput::from_symbol(&sym, &root())
        .with_signature(Some("fn process(&self)".to_string()));
    assert_json_snapshot!(out);
}

#[test]
fn target_output_fallback_for_unknown_symbol() {
    let out = TargetOutput::from_symbol_or_fallback(
        None,
        &root().join("src/main.rs"),
        50,
        4,
        &root(),
        Some("not_a_symbol"),
    );
    assert_json_snapshot!(out);
}

#[test]
fn ref_output_full_metadata() {
    let out = RefOutput {
        total: 42,
        test: 12,
        prod: 30,
        files: Some(8),
        modules: Some(3),
        is_exported: Some(true),
        indexing: None,
    };
    assert_json_snapshot!(out);
}

#[test]
fn ref_output_discloses_degraded_indexing() {
    // The reference counts come from a query that ran under a warming index, so
    // they are a lower bound — disclosed via `indexing`, omitted otherwise so
    // the common (authoritative) summary carries no filler.
    let out = RefOutput {
        total: 4,
        test: 1,
        prod: 3,
        files: Some(2),
        modules: Some(1),
        is_exported: Some(false),
        indexing: Some(IndexingDegradation::TimedOut),
    };
    let value = serde_json::to_value(out).unwrap();
    assert_eq!(value["indexing"], "timed_out");
}

#[test]
fn ref_output_minimal() {
    let out = RefOutput {
        total: 0,
        test: 0,
        prod: 0,
        files: None,
        modules: None,
        is_exported: None,
        indexing: None,
    };
    assert_json_snapshot!(out);
}

#[test]
fn type_info_output_with_detail() {
    let item = TypeHierarchyItem {
        name: "Iterator".to_string(),
        kind: SymbolKind::Interface,
        location: Location::point(root().join("src/iter.rs"), 5, 11),
        detail: Some("trait".to_string()),
    };
    let out = TypeInfoOutput::from_item(&item, &root());
    assert_json_snapshot!(out);
}

/// `body` appears on the type item only when `context --with-bodies`
/// admitted it — every other producer leaves it `None` (omitted).
#[test]
fn type_info_output_with_body() {
    let out = TypeInfoOutput {
        name: "Config".to_string(),
        kind: "struct".to_string(),
        location: sample_location(5, 12),
        detail: None,
        body: Some("struct Config { path: PathBuf }".to_string()),
    };
    assert_json_snapshot!(out, @r###"
    {
      "name": "Config",
      "kind": "struct",
      "location": {
        "file": "src/main.rs",
        "line": 5,
        "column": 12
      },
      "body": "struct Config { path: PathBuf }"
    }
    "###);
}

#[test]
fn test_output_basic() {
    let out = TestOutput {
        name: "test_process".to_string(),
        location: sample_location(80, 4),
    };
    assert_json_snapshot!(out);
}

#[test]
fn test_coverage_output_with_files() {
    let out = TestCoverageOutput {
        count: 3,
        files: vec![
            "tests/integration.rs".to_string(),
            "tests/api.rs".to_string(),
        ],
    };
    assert_json_snapshot!(out);
}

#[test]
fn affected_file_output() {
    let out = AffectedFileOutput {
        file: "src/handler.rs".to_string(),
        is_test: false,
        refs: 7,
    };
    assert_json_snapshot!(out);
}

#[test]
fn impact_output_full() {
    let target = TargetOutput::new(
        "process".to_string(),
        "function".to_string(),
        "src/main.rs".to_string(),
        12,
    );
    let out = ImpactOutput {
        target,
        refs: RefOutput {
            total: 10,
            test: 3,
            prod: 7,
            files: Some(4),
            modules: Some(2),
            is_exported: Some(true),
            indexing: None,
        },
        coverage: TestCoverageOutput {
            count: 3,
            files: vec!["tests/api.rs".to_string()],
        },
        files: vec![AffectedFileOutput {
            file: "src/handler.rs".to_string(),
            is_test: false,
            refs: 5,
        }],
        blast_radius: Some(BlastRadius {
            direct_callers: 4,
            transitive_callers: 9,
            depth: 2,
            max_depth_reached: false,
            callers_truncated: false,
            indexing: None,
            incomplete: false,
            dynamic_dispatch: None,
            callers_by_depth: vec![
                DepthBucket {
                    depth: 1,
                    count: 4,
                    test: 1,
                    prod: 3,
                },
                DepthBucket {
                    depth: 2,
                    count: 5,
                    test: 2,
                    prod: 3,
                },
            ],
            test_coverage_ratio: 0.33,
            risk: RiskLevel::Medium,
            confidence: 0.9,
        }),
        next_commands: vec!["symora impact src/main.rs:12:4 --depth 2".to_string()],
    };
    assert_json_snapshot!(out);
}

#[test]
fn impact_output_without_blast_radius() {
    let out = ImpactOutput {
        target: TargetOutput::new(
            "process".to_string(),
            "function".to_string(),
            "src/main.rs".to_string(),
            12,
        ),
        refs: RefOutput {
            total: 0,
            test: 0,
            prod: 0,
            files: None,
            modules: None,
            is_exported: None,
            indexing: None,
        },
        coverage: TestCoverageOutput {
            count: 0,
            files: vec![],
        },
        files: vec![],
        blast_radius: None,
        next_commands: vec![],
    };
    assert_json_snapshot!(out);
}

#[test]
fn blast_radius_max_depth_reached_serializes() {
    let radius = BlastRadius {
        direct_callers: 2,
        transitive_callers: 7,
        depth: 3,
        max_depth_reached: true,
        callers_truncated: true,
        indexing: Some(symora::models::lsp::IndexingDegradation::TimedOut),
        incomplete: false,
        dynamic_dispatch: None,
        callers_by_depth: vec![
            DepthBucket {
                depth: 1,
                count: 2,
                test: 0,
                prod: 2,
            },
            DepthBucket {
                depth: 2,
                count: 3,
                test: 1,
                prod: 2,
            },
            DepthBucket {
                depth: 3,
                count: 2,
                test: 0,
                prod: 2,
            },
        ],
        test_coverage_ratio: 0.14,
        risk: RiskLevel::High,
        confidence: 1.0,
    };
    assert_json_snapshot!(radius);
}

#[test]
fn risk_level_serializes_lowercase() {
    let levels = [
        RiskLevel::Low,
        RiskLevel::Medium,
        RiskLevel::High,
        RiskLevel::Critical,
    ];
    assert_json_snapshot!(levels, @r###"
    [
      "low",
      "medium",
      "high",
      "critical"
    ]
    "###);
}

#[test]
fn server_status_output_with_install_hint() {
    let out = ServerStatusOutput {
        language: "rust".to_string(),
        status: "missing".to_string(),
        error: Some("not installed".to_string()),
        install_hint: Some("rustup component add rust-analyzer".to_string()),
    };
    assert_json_snapshot!(out);
}

#[test]
fn file_change_output() {
    let out = FileChangeOutput {
        file: "src/api.rs".to_string(),
        edit_count: 4,
    };
    assert_json_snapshot!(out);
}

#[test]
fn signature_help_output_full() {
    let out = SignatureHelpOutput {
        signatures: vec![SignatureItemOutput {
            label: "fn process(&self, name: &str) -> Result<()>".to_string(),
            documentation: Some("Process the request.".to_string()),
            parameters: vec![
                ParameterOutput {
                    label: "&self".to_string(),
                    documentation: None,
                },
                ParameterOutput {
                    label: "name: &str".to_string(),
                    documentation: Some("the name".to_string()),
                },
            ],
            active_parameter: Some(1),
        }],
        active_signature: Some(0),
        active_parameter: Some(1),
        message: None,
    };
    assert_json_snapshot!(out);
}

#[test]
fn action_output_with_diagnostics() {
    let out = ActionOutput {
        title: "Import Foo".to_string(),
        kind: "quickfix".to_string(),
        is_preferred: true,
        diagnostics: vec!["E0432".to_string()],
    };
    assert_json_snapshot!(out);
}

#[test]
fn apply_action_output_success() {
    let out = ApplyActionOutput {
        title: "Import Foo".to_string(),
        kind: "quickfix".to_string(),
        applied: true,
        files_changed: 1,
        changes: vec![FileChangeOutput {
            file: "src/main.rs".to_string(),
            edit_count: 1,
        }],
        message: None,
    };
    assert_json_snapshot!(out);
}

#[test]
fn error_envelope_shape_matches_runtime() {
    // Mirrors what OutputContext::print_error emits to stdout.
    // Note: serde_json is built with `preserve_order`, so object keys keep
    // insertion order — here the `OutputError` field order (code, message,
    // hint) — which is exactly what the runtime prints.
    let err = OutputError::not_found("symbol foo").with_hint("did you mean 'fool'?");
    let envelope = json!({ "error": err });
    assert_json_snapshot!(envelope, @r###"
    {
      "error": {
        "code": "not_found",
        "message": "symbol foo",
        "hint": "did you mean 'fool'?"
      }
    }
    "###);
}

/// Symbol-targeted edit: target fields present, applied run omits
/// `dry_run` and `preview` entirely.
#[test]
fn edit_output_symbol_applied() {
    let out = EditOutput {
        operation: "replace_body",
        file: "src/main.rs".to_string(),
        target_symbol: Some("Handler/process".to_string()),
        target_kind: Some("function".to_string()),
        lines: LineRange { start: 12, end: 20 },
        bytes_changed: -34,
        dry_run: false,
        preview: None,
        dangling_references: None,
        references_status: None,
        diagnostics: None,
        caller_verification: None,
    };
    assert_json_snapshot!(out, @r###"
    {
      "operation": "replace_body",
      "file": "src/main.rs",
      "target_symbol": "Handler/process",
      "target_kind": "function",
      "lines": {
        "start": 12,
        "end": 20
      },
      "bytes_changed": -34
    }
    "###);
}

/// Raw range edit: no symbol fields; dry runs carry `dry_run` and the
/// exact preview hunk.
#[test]
fn edit_output_range_dry_run() {
    let out = EditOutput {
        operation: "replace",
        file: "src/main.rs".to_string(),
        target_symbol: None,
        target_kind: None,
        lines: LineRange { start: 3, end: 3 },
        bytes_changed: 2,
        dry_run: true,
        preview: Some("@@ -3,1 +3,1 @@\n-old\n+older\n".to_string()),
        dangling_references: None,
        references_status: None,
        diagnostics: None,
        caller_verification: None,
    };
    assert_json_snapshot!(out, @r###"
    {
      "operation": "replace",
      "file": "src/main.rs",
      "lines": {
        "start": 3,
        "end": 3
      },
      "bytes_changed": 2,
      "dry_run": true,
      "preview": "@@ -3,1 +3,1 @@\n-old\n+older\n"
    }
    "###);
}

/// Delete: the reference check is explicit even when it finds nothing —
/// `count: 0` is a real answer, absence would be ambiguous with
/// "couldn't check".
#[test]
fn edit_output_delete_with_clean_reference_check() {
    let out = EditOutput {
        operation: "delete",
        file: "src/main.rs".to_string(),
        target_symbol: Some("helper".to_string()),
        target_kind: Some("function".to_string()),
        lines: LineRange { start: 5, end: 7 },
        bytes_changed: -42,
        dry_run: false,
        preview: None,
        dangling_references: Some(Section::new(vec![sample_location(50, 12)])),
        references_status: None,
        diagnostics: None,
        caller_verification: None,
    };
    assert_json_snapshot!(out, @r###"
    {
      "operation": "delete",
      "file": "src/main.rs",
      "target_symbol": "helper",
      "target_kind": "function",
      "lines": {
        "start": 5,
        "end": 7
      },
      "bytes_changed": -42,
      "dangling_references": {
        "count": 1,
        "showing": 1,
        "items": [
          {
            "file": "src/main.rs",
            "line": 50,
            "column": 12
          }
        ]
      }
    }
    "###);
}

#[test]
fn language_serializes_as_lowercase_id() {
    // Internal model used by some response wrappers — pin its id form.
    assert_eq!(Language::Rust.lsp_id(), "rust");
    assert_eq!(Language::TypeScript.lsp_id(), "typescript");
    assert_eq!(Language::CSharp.lsp_id(), "csharp");
}
