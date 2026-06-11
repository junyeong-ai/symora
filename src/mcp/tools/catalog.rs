use crate::mcp::protocol::ToolDefinition;

use super::schema::{
    edit_target_schema, location_schema, schema_object, section_output_schema, with_extra,
};

pub fn build_catalog() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::read_only(
            "get_project_overview",
            "High-level project map: language breakdown, top directories, and \
                          probable entrypoints. Cheap orientation step before deeper queries.",
            schema_object(&[], &[]),
        ),
        ToolDefinition::read_only(
            "get_file_overview",
            "Compact map for one file: focus symbols, sibling/counterpart files, \
                          shallow symbol tree, and related-file ranking.",
            schema_object(
                &[
                    ("path", "string", "Project-relative file path"),
                    ("depth", "integer", "Nested-symbol depth (default 1)"),
                    (
                        "related_limit",
                        "integer",
                        "Maximum related files to show (default 8)",
                    ),
                ],
                &["path"],
            ),
        ),
        ToolDefinition::read_only(
            "search_symbols",
            "Fast rough symbol discovery by name or path-like pattern across the \
                          workspace. Use this when you don't yet know the exact file.",
            schema_object(
                &[
                    ("query", "string", "Partial symbol name or path pattern"),
                    (
                        "language",
                        "string",
                        "Optional language filter (rust, python, ...)",
                    ),
                    (
                        "kind",
                        "string",
                        "Optional kind filter (function, class, struct, ...)",
                    ),
                    ("limit", "integer", "Maximum results"),
                ],
                &["query"],
            ),
        )
        .with_output_schema(section_output_schema(
            "Symbol matches: name, name_path, kind, file, line, column, score",
        )),
        ToolDefinition::read_only(
            "search_content",
            "Fast keyword/phrase search across indexed file contents.",
            schema_object(
                &[
                    ("query", "string", "Keyword or phrase"),
                    ("language", "string", "Optional language filter"),
                    ("limit", "integer", "Maximum results"),
                ],
                &["query"],
            ),
        )
        .with_output_schema(section_output_schema(
            "Content-line matches: file, line, content, score",
        )),
        ToolDefinition::read_only(
            "list_file_symbols",
            "List symbols defined in one file (precise, not heuristic). Use after \
                          file_overview when you need the full symbol tree.",
            schema_object(
                &[
                    ("file", "string", "Project-relative file path"),
                    ("depth", "integer", "Nested-symbol depth (0 = top level)"),
                    ("body", "boolean", "Include source bodies"),
                    ("signature", "boolean", "Include signatures only"),
                ],
                &["file"],
            ),
        )
        .with_output_schema(section_output_schema("Symbols defined in the file")),
        ToolDefinition::read_only(
            "inspect_symbol",
            "Resolve an exact symbol path (e.g., 'Handler/process') and return its \
                          definition info. Use after search_symbols to follow up precisely.",
            schema_object(
                &[
                    ("symbol_path", "string", "Symbol path like 'Class/method'"),
                    ("language", "string", "Optional language filter"),
                    ("body", "boolean", "Include source body"),
                ],
                &["symbol_path"],
            ),
        ),
        ToolDefinition::read_only(
            "find_definition",
            "LSP go-to-definition for the symbol at a precise file:line:column.",
            location_schema(),
        ),
        ToolDefinition::read_only(
            "find_references",
            "All references to the symbol at a precise file:line:column.",
            with_extra(
                location_schema(),
                &[
                    ("snippet", "boolean", "Include source snippets"),
                    ("limit", "integer", "Maximum results"),
                ],
            ),
        )
        .with_output_schema(with_extra(
            section_output_schema("Reference locations: file, line, column"),
            &[(
                "target",
                "object",
                "The symbol the position resolved to: { name, kind, file, line, signature?, resolved }",
            )],
        )),
        ToolDefinition::read_only(
            "find_callers",
            "Incoming-call hierarchy for a function at a precise file:line:column.",
            with_extra(
                location_schema(),
                &[("limit", "integer", "Maximum results")],
            ),
        )
        .with_output_schema(with_extra(
            section_output_schema("Incoming calls with caller locations"),
            &[(
                "callers_status",
                "string",
                "Present only when callers were derived from references because the \
                 server lacks call hierarchy — reference-based, not verified call edges",
            )],
        )),
        ToolDefinition::read_only(
            "find_callees",
            "Outgoing-call hierarchy for a function at a precise file:line:column.",
            with_extra(
                location_schema(),
                &[("limit", "integer", "Maximum results")],
            ),
        )
        .with_output_schema(section_output_schema(
            "Outgoing calls with callee locations",
        )),
        ToolDefinition::read_only(
            "find_implementations",
            "All concrete implementations of a trait/interface at a precise \
                          file:line:column.",
            with_extra(
                location_schema(),
                &[("limit", "integer", "Maximum results")],
            ),
        )
        .with_output_schema(section_output_schema(
            "Implementation locations: file, line, column",
        )),
        ToolDefinition::read_only(
            "get_hover",
            "Hover documentation/type for the symbol at a precise file:line:column.",
            location_schema(),
        ),
        ToolDefinition::read_only(
            "get_context",
            "Aggregated context for a symbol at file:line:column — by default \
                          callers, callees, related types, and tests in one response.",
            with_extra(
                location_schema(),
                &[
                    (
                        "callers",
                        "boolean",
                        "Include incoming calls (default with all)",
                    ),
                    (
                        "callees",
                        "boolean",
                        "Include outgoing calls (default with all)",
                    ),
                    ("types", "boolean", "Include type definitions used"),
                    ("tests", "boolean", "Include related tests"),
                    (
                        "all",
                        "boolean",
                        "Include every context section (default true)",
                    ),
                ],
            ),
        ),
        ToolDefinition::read_only(
            "get_impact",
            "Change-impact analysis at file:line:column: reference counts split by \
                          test vs prod, affected files, exported-API signal, and a transitive \
                          caller graph with risk + confidence (`blast_radius`). Use depth=1 for \
                          quick surveys, depth=2-3 when ranking blast radius matters.",
            with_extra(
                location_schema(),
                &[
                    ("limit", "integer", "Maximum affected files to list"),
                    (
                        "depth",
                        "integer",
                        "Transitive caller depth, 1-3 (default 1)",
                    ),
                ],
            ),
        ),
        ToolDefinition::read_only(
            "build_context_pack",
            "Build a token-budgeted context pack: PageRank-ranked files with \
                          top-level signatures fitted to a token budget. Strong first call \
                          when starting a new task in this repo. Set shape=\"markdown\" for a \
                          plain-text view ready to paste into an LLM context window.",
            schema_object(
                &[
                    (
                        "tokens",
                        "integer",
                        "Approximate token budget (default 4000)",
                    ),
                    (
                        "focus",
                        "string",
                        "Optional file path or substring to bias the ranking",
                    ),
                    (
                        "per_file",
                        "integer",
                        "Cap on top-level symbols per file (default 12)",
                    ),
                    (
                        "shape",
                        "string",
                        "Output shape: \"json\" (default) or \"markdown\"",
                    ),
                ],
                &[],
            ),
        ),
        ToolDefinition::mutating(
            "rename_symbol",
            "LSP rename for the symbol at file:line:column. Returns the affected \
                          file list and per-file edit count. Set dry_run=true to preview \
                          without writing. ⚠ Mutates source files when dry_run is false.",
            with_extra(
                location_schema(),
                &[
                    ("new_name", "string", "Replacement identifier"),
                    (
                        "dry_run",
                        "boolean",
                        "Preview the rename without writing (default false)",
                    ),
                ],
            ),
        ),
        ToolDefinition::read_only(
            "list_code_actions",
            "List LSP code actions available at file:line:column \
                          (refactor/quickfix/source). Filter with `kind` or `preferred=true`.",
            with_extra(
                location_schema(),
                &[
                    ("kind", "string", "Filter by action kind"),
                    ("preferred", "boolean", "Only preferred actions"),
                ],
            ),
        )
        .with_output_schema(section_output_schema("Available code actions")),
        ToolDefinition::mutating(
            "apply_code_action",
            "Apply a code action by exact title at file:line:column. Use \
                          `list_code_actions` first to discover titles. Set dry_run=true to \
                          preview. ⚠ Mutates source files when dry_run is false.",
            with_extra(
                location_schema(),
                &[
                    ("title", "string", "Exact action title to apply"),
                    (
                        "dry_run",
                        "boolean",
                        "Preview the change without writing (default false)",
                    ),
                ],
            ),
        ),
        ToolDefinition::mutating(
            "replace_symbol_body",
            "Replace the resolved symbol's full body with new source code. \
                          Target by file + symbol path (e.g. 'Class/method') or by \
                          file:line:column — exactly one of symbol or line. Splices by the \
                          LSP's symbol range so braces / decorators stay intact. \
                          ⚠ Mutates source files when dry_run is false.",
            with_extra(
                edit_target_schema(),
                &[
                    ("body", "string", "New source for the symbol"),
                    (
                        "dry_run",
                        "boolean",
                        "Preview the change without writing (default false)",
                    ),
                    (
                        "with_diagnostics",
                        "boolean",
                        "Attach post-edit LSP diagnostics (default false)",
                    ),
                ],
            ),
        ),
        ToolDefinition::mutating(
            "insert_before_symbol",
            "Insert source code immediately before the symbol, targeted by \
                          file + symbol path or file:line:column — exactly one of symbol or \
                          line. ⚠ Mutates source files when dry_run is false.",
            with_extra(
                edit_target_schema(),
                &[
                    ("code", "string", "Source code to insert"),
                    (
                        "dry_run",
                        "boolean",
                        "Preview without writing (default false)",
                    ),
                    (
                        "with_diagnostics",
                        "boolean",
                        "Attach post-edit LSP diagnostics (default false)",
                    ),
                ],
            ),
        ),
        ToolDefinition::mutating(
            "insert_after_symbol",
            "Insert source code immediately after the symbol, targeted by \
                          file + symbol path or file:line:column — exactly one of symbol or \
                          line. ⚠ Mutates source files when dry_run is false.",
            with_extra(
                edit_target_schema(),
                &[
                    ("code", "string", "Source code to insert"),
                    (
                        "dry_run",
                        "boolean",
                        "Preview without writing (default false)",
                    ),
                    (
                        "with_diagnostics",
                        "boolean",
                        "Attach post-edit LSP diagnostics (default false)",
                    ),
                ],
            ),
        ),
        ToolDefinition::mutating(
            "delete_symbol",
            "Delete the symbol's full definition. Target by file + symbol \
                          path (e.g. 'Class/method') or by file:line:column — exactly one of \
                          symbol or line. Always reports references outside the deleted span \
                          that would dangle (report-only). Set dry_run=true to preview. \
                          ⚠ Mutates source files when dry_run is false.",
            with_extra(
                edit_target_schema(),
                &[
                    (
                        "dry_run",
                        "boolean",
                        "Preview the deletion without writing (default false)",
                    ),
                    (
                        "with_diagnostics",
                        "boolean",
                        "Attach post-edit LSP diagnostics (default false)",
                    ),
                ],
            ),
        ),
    ]
}
