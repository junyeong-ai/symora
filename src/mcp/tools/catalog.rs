use crate::mcp::protocol::ToolDefinition;

use super::schema::{location_schema, schema_object, with_extra};

pub fn build_catalog() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "get_project_overview",
            description: "High-level project map: language breakdown, top directories, and \
                          probable entrypoints. Cheap orientation step before deeper queries.",
            input_schema: schema_object(&[], &[]),
        },
        ToolDefinition {
            name: "get_file_overview",
            description: "Compact map for one file: focus symbols, sibling/counterpart files, \
                          shallow symbol tree, and related-file ranking.",
            input_schema: schema_object(
                &[("path", "string", "Project-relative file path")],
                &["path"],
            ),
        },
        ToolDefinition {
            name: "search_symbols",
            description: "Fast rough symbol discovery by name or path-like pattern across the \
                          workspace. Use this when you don't yet know the exact file.",
            input_schema: schema_object(
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
        },
        ToolDefinition {
            name: "search_content",
            description: "Fast keyword/phrase search across indexed file contents.",
            input_schema: schema_object(
                &[
                    ("query", "string", "Keyword or phrase"),
                    ("language", "string", "Optional language filter"),
                    ("limit", "integer", "Maximum results"),
                ],
                &["query"],
            ),
        },
        ToolDefinition {
            name: "list_file_symbols",
            description: "List symbols defined in one file (precise, not heuristic). Use after \
                          file_overview when you need the full symbol tree.",
            input_schema: schema_object(
                &[
                    ("file", "string", "Project-relative file path"),
                    ("depth", "integer", "Nested-symbol depth (0 = top level)"),
                    ("body", "boolean", "Include source bodies"),
                    ("signature", "boolean", "Include signatures only"),
                ],
                &["file"],
            ),
        },
        ToolDefinition {
            name: "inspect_symbol",
            description: "Resolve an exact symbol path (e.g., 'Handler/process') and return its \
                          definition info. Use after search_symbols to follow up precisely.",
            input_schema: schema_object(
                &[
                    ("symbol_path", "string", "Symbol path like 'Class/method'"),
                    ("language", "string", "Optional language filter"),
                    ("body", "boolean", "Include source body"),
                ],
                &["symbol_path"],
            ),
        },
        ToolDefinition {
            name: "find_definition",
            description: "LSP go-to-definition for the symbol at a precise file:line:column.",
            input_schema: location_schema(),
        },
        ToolDefinition {
            name: "find_references",
            description: "All references to the symbol at a precise file:line:column.",
            input_schema: with_extra(
                location_schema(),
                &[
                    ("snippet", "boolean", "Include source snippets"),
                    ("limit", "integer", "Maximum results"),
                ],
            ),
        },
        ToolDefinition {
            name: "find_callers",
            description: "Incoming-call hierarchy for a function at a precise file:line:column.",
            input_schema: with_extra(
                location_schema(),
                &[("limit", "integer", "Maximum results")],
            ),
        },
        ToolDefinition {
            name: "find_callees",
            description: "Outgoing-call hierarchy for a function at a precise file:line:column.",
            input_schema: with_extra(
                location_schema(),
                &[("limit", "integer", "Maximum results")],
            ),
        },
        ToolDefinition {
            name: "find_implementations",
            description: "All concrete implementations of a trait/interface at a precise \
                          file:line:column.",
            input_schema: with_extra(
                location_schema(),
                &[("limit", "integer", "Maximum results")],
            ),
        },
        ToolDefinition {
            name: "get_hover",
            description: "Hover documentation/type for the symbol at a precise file:line:column.",
            input_schema: location_schema(),
        },
        ToolDefinition {
            name: "get_context",
            description: "Aggregated context for a symbol at file:line:column — by default \
                          callers, callees, related types, and tests in one response.",
            input_schema: with_extra(
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
        },
        ToolDefinition {
            name: "get_impact",
            description: "Change-impact analysis at file:line:column: reference counts split by \
                          test vs prod, affected files, exported-API signal, and a transitive \
                          caller graph with risk + confidence (`blast_radius`). Use depth=1 for \
                          quick surveys, depth=2-3 when ranking blast radius matters.",
            input_schema: with_extra(
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
        },
        ToolDefinition {
            name: "build_context_pack",
            description: "Build a token-budgeted context pack: PageRank-ranked files with \
                          top-level signatures fitted to a token budget. Strong first call \
                          when starting a new task in this repo. Set shape=\"markdown\" for a \
                          plain-text view ready to paste into an LLM context window.",
            input_schema: schema_object(
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
        },
        ToolDefinition {
            name: "rename_symbol",
            description: "LSP rename for the symbol at file:line:column. Returns the affected \
                          file list and per-file edit count. Set dry_run=true to preview \
                          without writing. ⚠ Mutates source files when dry_run is false.",
            input_schema: with_extra(
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
        },
        ToolDefinition {
            name: "list_code_actions",
            description: "List LSP code actions available at file:line:column \
                          (refactor/quickfix/source). Filter with `kind` or `preferred=true`.",
            input_schema: with_extra(
                location_schema(),
                &[
                    ("kind", "string", "Filter by action kind"),
                    ("preferred", "boolean", "Only preferred actions"),
                ],
            ),
        },
        ToolDefinition {
            name: "apply_code_action",
            description: "Apply a code action by exact title at file:line:column. Use \
                          `list_code_actions` first to discover titles. Set dry_run=true to \
                          preview. ⚠ Mutates source files when dry_run is false.",
            input_schema: with_extra(
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
        },
        ToolDefinition {
            name: "replace_symbol_body",
            description: "Replace the resolved symbol's full body with new source code at \
                          file:line:column. Splices by the LSP's symbol range so braces / \
                          decorators stay intact. ⚠ Mutates source files when dry_run is false.",
            input_schema: with_extra(
                location_schema(),
                &[
                    ("body", "string", "New source for the symbol"),
                    (
                        "dry_run",
                        "boolean",
                        "Preview the change without writing (default false)",
                    ),
                ],
            ),
        },
        ToolDefinition {
            name: "insert_before_symbol",
            description: "Insert source code immediately before the symbol at \
                          file:line:column. ⚠ Mutates source files when dry_run is false.",
            input_schema: with_extra(
                location_schema(),
                &[
                    ("code", "string", "Source code to insert"),
                    (
                        "dry_run",
                        "boolean",
                        "Preview without writing (default false)",
                    ),
                ],
            ),
        },
        ToolDefinition {
            name: "insert_after_symbol",
            description: "Insert source code immediately after the symbol at \
                          file:line:column. ⚠ Mutates source files when dry_run is false.",
            input_schema: with_extra(
                location_schema(),
                &[
                    ("code", "string", "Source code to insert"),
                    (
                        "dry_run",
                        "boolean",
                        "Preview without writing (default false)",
                    ),
                ],
            ),
        },
    ]
}
