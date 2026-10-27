use crate::constants::defaults::{
    CALL_GRAPH_MAX_DEPTH, CONTEXT_BODY_TOKENS, DIFF_IMPACT_MAX_SYMBOLS, IMPACT_DEFAULT_DEPTH,
    MAP_FILE_DEPTH, MAP_FILE_RELATED_LIMIT, PACK_SYMBOLS_PER_FILE, PACK_TOKENS,
};
use crate::mcp::protocol::ToolDefinition;

use super::schema::{
    edit_target_schema, location_schema, output_schema, position_exact_location_schema,
    schema_object, section_output_schema, with_extra, with_required,
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
            "Compact map for one file: focus symbols, siblings, name-matched counterparts, \
                          shallow symbol tree, and nearby files suggested from names and \
                          directory layout. Use find_references for semantic links. `focus_symbols` and \
                          `backend` are omitted together when nothing read the file's \
                          declarations — an empty list would say the file declares none.",
            schema_object(
                &[
                    ("path", "string", "Project-relative file path"),
                    (
                        "depth",
                        "integer",
                        &format!("Nested-symbol depth (default {MAP_FILE_DEPTH})"),
                    ),
                    (
                        "related_limit",
                        "integer",
                        &format!("Maximum related files to show (default {MAP_FILE_RELATED_LIMIT})"),
                    ),
                ],
                &["path"],
            ),
        ),
        ToolDefinition::read_only(
            "search_symbols",
            "Symbol discovery by name or path-like pattern (e.g. 'Handler/process', \
                          '*/method') across the workspace, for when you don't know the file. \
                          body or signature reads each match's source, so a known declaration \
                          needs no second call.",
            {
                let mut schema = schema_object(
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
                        (
                            "body",
                            "boolean",
                            "Attach each match's complete definition and signature",
                        ),
                        ("signature", "boolean", "Attach each match's signature"),
                        ("limit", "integer", "Maximum results"),
                    ],
                    &["query"],
                );
                schema["properties"]["path"] = serde_json::json!({
                    "type": "array", "items": {"type": "string"}, "minItems": 1,
                    "description": "Project-relative files/directories whose declarations to search"
                });
                schema
            },
        )
        .with_output_schema(with_extra(
            section_output_schema(
                "Symbol matches: name, name_path, kind, file, line, column, score, and \
                 signature/body when requested",
            ),
            &[(
                "coverage_gaps",
                "array",
                "Languages this answer could not vouch for: [{language, reason, message?}], \
                 reason one of not_indexed | server_not_installed | not_consulted | unavailable | \
                 timed_out | unsupported | not_searched; an unavailable gap carries message, the \
                 failure's first line. Present whether items is empty or partial — a language listed \
                 here was not covered, so its absence from items proves nothing. Reach it with \
                 search_content/AST, or fix what the reason names",
            )],
        )),
        ToolDefinition::read_only(
            "check_language_support",
            "What Symora can answer for in this workspace, per language: whether the \
                          language server serves it, and whether the index extractor and \
                          tree-sitter grammar cover it with no server installed. Branch on this \
                          instead of discovering a language's limits by a failed call.",
            schema_object(
                &[(
                    "language",
                    "string",
                    "Optional single language to report on (rust, python, ...)",
                )],
                &[],
            ),
        )
        .with_output_schema(output_schema(&[
            ("platform", "string", "Host platform the report was taken on"),
            (
                "languages",
                "array",
                "Per language: server, installed, serves, symbol_extraction, ast_search, and \
                 install when it does not serve. `serves` is the capability — `installed` only \
                 says a file resolves at that path, which a version-manager shim satisfies too",
            ),
            ("summary", "object", "total / serving / missing"),
            (
                "config_errors",
                "array",
                "Config problems affecting this report, omitted when there are none",
            ),
        ])),
        ToolDefinition::read_only(
            "search_ast",
            "Structural search by tree-sitter query — the grammar's own node shapes, not \
                          text. Answers for languages no symbol extractor or language server \
                          covers, and finds forms a name cannot express.",
            {
                let mut schema = schema_object(
                    &[
                        (
                            "pattern",
                            "string",
                            "Tree-sitter query, e.g. \"(function_definition) @f\"",
                        ),
                        ("language", "string", "Language the grammar belongs to"),
                        ("limit", "integer", "Maximum results"),
                    ],
                    &["pattern", "language"],
                );
                schema["properties"]["path"] = serde_json::json!({
                    "type": "array", "items": {"type": "string"}, "minItems": 1,
                    "description": "Project-relative files/directories to search, including hidden paths"
                });
                schema
            },
        )
        .with_output_schema(section_output_schema(
            "Matches: file, start_line, end_line, start_column, end_column, text, captures",
        )),
        ToolDefinition::read_only(
            "get_index_status",
            "Whether the symbol/content index is built and what it covers. An unbuilt or \
                          narrowed index changes what search_symbols and search_content can \
                          answer, so read this before treating a zero as absence.",
            schema_object(&[], &[]),
        )
        .with_output_schema(output_schema(&[
            ("file_count", "integer", "Files the index holds rows for"),
            ("symbol_count", "integer", "Symbol rows"),
            ("content_line_count", "integer", "Content lines"),
            ("index_size_bytes", "integer", "Logical size of the store"),
            ("last_indexed", "integer", "Unix time of the last completed build, 0 if never"),
            ("is_indexing", "boolean", "Whether a build is running now"),
            (
                "languages",
                "array",
                "What the last completed build extracts symbols for; empty means no build has \
                 completed, and a symbol search is complete only for the languages listed",
            ),
            (
                "unread_paths",
                "array",
                "Paths that build could not read, omitted when there were none",
            ),
        ])),
        ToolDefinition::mutating(
            "build_index",
            "Mutates the project's search index (.symora/), never source. Build or refresh \
                          it so symbol and content search answer from the index rather than a \
                          live fan-out — the remedy get_index_status and a not_indexed coverage \
                          gap both name.",
            schema_object(
                &[
                    ("force", "boolean", "Rebuild from scratch instead of in place"),
                    (
                        "languages",
                        "string",
                        "Comma-separated languages to narrow the build to",
                    ),
                ],
                &[],
            ),
        )
        .with_output_schema(output_schema(&[
            ("status", "string", "Build outcome"),
            ("file_count", "integer", "Files indexed"),
            ("symbol_count", "integer", "Symbol rows written"),
            ("content_line_count", "integer", "Content lines written"),
            (
                "unread_paths",
                "array",
                "Paths the build could not read, omitted when there were none",
            ),
        ])),
        ToolDefinition::read_only(
            "search_content",
            "Keyword/phrase search. Defaults to code files; language selects documentation/data formats too. Explicit path searches read the named files/directories live, including hidden paths and text formats. Project ignores and size limits apply; .git and .symora are excluded. Unscoped queries use the index and uncovered live sources; zero matches are confirmed against disk.",
            {
                let mut schema = schema_object(&[
                    ("query", "string", "Keyword or phrase"),
                    ("language", "string", "Optional language filter"),
                    ("limit", "integer", "Maximum results"),
                ], &["query"]);
                schema["properties"]["path"] = serde_json::json!({
                    "type": "array", "items": {"type": "string"}, "minItems": 1,
                    "description": "Explicit project-relative files/directories to search live, including hidden paths"
                });
                schema
            },
        ).with_output_schema(section_output_schema("Content-line matches: file, line, content, score")),
        ToolDefinition::read_only(
            "list_file_symbols",
            "List declarations in a known file, with optional bodies, signatures, or \
             reference counts. Reports backend: document for the language server's nested \
             tree, or ast for flat syntax-derived declarations with name_path; neither lists \
             the parameters and locals a function, method or constructor binds. refs counts \
             each listed declaration's references, other than what a function declares; \
             depth 1 lists a type's members. A zero is static and does not show that nothing \
             reaches a declaration.",
            schema_object(
                &[
                    ("file", "string", "Project-relative file path"),
                    ("depth", "integer", "Nested-symbol depth (0 = top level)"),
                    ("body", "boolean", "Include source bodies"),
                    ("signature", "boolean", "Include signatures only"),
                    (
                        "refs",
                        "boolean",
                        "Add refs, test_refs and prod_refs to each counted declaration, with \
                         refs_status when a count is unavailable or a lower bound",
                    ),
                ],
                &["file"],
            ),
        )
        .with_output_schema(with_extra(
            section_output_schema("Symbols defined in the file"),
            &[(
                "backend",
                "string",
                "What read the file: `document` (the language server, nested, its own kinds) or \
                 `ast` (the compiled-in grammar, flat, containment in name_path). Read it before \
                 taking absent nesting for structure",
            )],
        )),
        ToolDefinition::read_only(
            "find_definition",
            "LSP go-to-definition for the symbol at a precise file:line:column.",
            position_exact_location_schema(),
        ),
        ToolDefinition::read_only(
            "find_references",
            "References to the symbol at file:line[:column]. Check indexing before \
             treating the count as complete; unconfirmed does not verify workspace readiness.",
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
            &[
                (
                    "target",
                    "object",
                    "The symbol the position resolved to: { name, kind, file, line, signature?, \
                     anchor_status? } — anchor_status is \"binding\" (a declaration the \
                     symbol tree does not list, such as a local or a parameter; the list is \
                     exactly its), \
                     \"not_a_symbol\", or \"unavailable\" only when the position did not \
                     resolve to a listed symbol, omitted when resolved",
                ),
                (
                    "incomplete",
                    "boolean",
                    "Present (true) only when the language server's reference set omits the \
                     very usage the query was made from; the list and count are then a lower \
                     bound",
                ),
            ],
        )),
        ToolDefinition::read_only(
            "find_callers",
            "Incoming-call hierarchy for a function at file:line[:column].",
            with_extra(
                location_schema(),
                &[
                    ("limit", "integer", "Maximum results"),
                    (
                        "no_fallback",
                        "boolean",
                        "Refuse the references-derived approximation when the server lacks call \
                         hierarchy (default false). With it true, a weak server yields a structured \
                         unsupported result instead of reference-based, non-verified call edges.",
                    ),
                ],
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
            "Outgoing-call hierarchy for a function at file:line[:column].",
            with_extra(
                location_schema(),
                &[
                    ("limit", "integer", "Maximum results"),
                    (
                        "depth",
                        "integer",
                        &format!(
                            "Transitive callee depth, 1-{CALL_GRAPH_MAX_DEPTH} (default 1; >1 \
                             returns the downward reachable set instead of direct callees, with \
                             honest lower-bound markers)"
                        ),
                    ),
                ],
            ),
        )
        .with_output_schema(with_extra(
            section_output_schema("Outgoing calls with callee locations"),
            &[
                (
                    "depth",
                    "integer",
                    "Depth actually reached (present only with depth>1, the reachable-set mode)",
                ),
                (
                    "max_depth_reached",
                    "boolean",
                    "Walk stopped at the depth cap with callees unexplored — the set is a \
                     lower bound (depth>1 only)",
                ),
                (
                    "callees_truncated",
                    "boolean",
                    "A node's fan-out hit the per-node cap, so the set is a lower bound \
                     (depth>1 only)",
                ),
                (
                    "incomplete",
                    "boolean",
                    "A hop's callee query failed and was treated as empty, so the set is a \
                     lower bound (depth>1 only)",
                ),
                (
                    "anchor_status",
                    "string",
                    "Present when the from-position did not resolve to a listed symbol: \
                     \"binding\" (a declaration the symbol tree does not list, such as a local or \
                     a parameter; the set is exactly its), \
                     \"not_a_symbol\" (read OK, denotes nothing) or \"unavailable\" (a read \
                     failed); for the last two an empty set is not authoritative (depth>1 only)",
                ),
            ],
        )),
        ToolDefinition::read_only(
            "find_call_path",
            "Shortest call path FROM a function (file:line[:column]) TO a target (`to`): \
                          how does the anchor reach that function? Returns the ordered call \
                          chain when reachable, or an honest 3-state verdict (found / \
                          not_reached_within_bound / no_static_path) that never overclaims — \
                          dynamic dispatch stays a disclosed lower bound, never an absolute \
                          'unreachable'.",
            with_required(
                with_extra(
                    location_schema(),
                    &[
                        (
                            "to",
                            "string",
                            "Target location (file:line[:column]) the call chain is sought to",
                        ),
                        (
                            "depth",
                            "integer",
                            &format!(
                                "Max search depth, 1-{CALL_GRAPH_MAX_DEPTH} (default \
                                 {CALL_GRAPH_MAX_DEPTH})"
                            ),
                        ),
                    ],
                ),
                &["to"],
            ),
        )
        .with_output_schema(output_schema(&[
            (
                "target",
                "object",
                "The target the chain was sought to (file, line, column)",
            ),
            (
                "reachability",
                "string",
                "found | not_reached_within_bound | no_static_path",
            ),
            (
                "dynamic_dispatch_possible",
                "boolean",
                "Present (true) only when reachability is no_static_path: the static call \
                     graph was fully explored, but dynamic dispatch (trait/virtual/callback \
                     calls) is not analyzed, so the target may still be reachable at runtime — \
                     never read no_static_path as an absolute unreachable",
            ),
            (
                "chain",
                "array",
                "Ordered call frames from the first hop through the target; present \
                     only when reachability is found",
            ),
            ("depth", "integer", "Depth actually reached"),
            (
                "max_depth_reached",
                "boolean",
                "Search stopped at the depth cap (the negative is a lower bound)",
            ),
            (
                "callees_truncated",
                "boolean",
                "A node's fan-out hit the per-node cap (lower bound)",
            ),
            (
                "incomplete",
                "boolean",
                "A hop's call query failed and was treated as empty, so the \
                     verdict is a lower bound — never an exhaustive negative",
            ),
            (
                "target_status",
                "string",
                "Present when the `to` target did not resolve to a listed symbol: \
                     \"binding\" (the verdict is about that binding), \"not_a_symbol\" or \
                     \"unavailable\" (the verdict is not authoritative, forced to \
                     not_reached_within_bound)",
            ),
            (
                "anchor_status",
                "string",
                "Present when the from-position did not resolve to a listed symbol: \
                     \"binding\" (the verdict is about that binding), \"not_a_symbol\" or \
                     \"unavailable\" (the verdict is not authoritative)",
            ),
            (
                "indexing",
                "string",
                "Present when a hop ran under degraded workspace indexing (lower bound)",
            ),
            (
                "hints",
                "array",
                "Optional next-step suggestions for the agent",
            ),
        ])),
        ToolDefinition::read_only(
            "find_implementations",
            "Language-server implementations of a trait/interface at file:line[:column]. \
             Structural conformance and runtime dispatch may not be enumerated.",
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
            position_exact_location_schema(),
        ),
        ToolDefinition::read_only(
            "get_context",
            "Aggregated symbol context: callers, callees, related types, and tests. Set with_bodies=true for complete source bodies sharing body_tokens in target/callee/type order. Bodies that do not fit are omitted with body_omitted; source-size fitting can also omit bodies. bodies_included counts emitted section bodies. Missing bodies can be unresolvable or absent in source.",
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
                    (
                        "with_bodies",
                        "boolean",
                        "Attach complete bodies under the shared body_tokens budget; target first, then callees and types (default false)",
                    ),
                    (
                        "body_tokens",
                        "integer",
                        &format!(
                            "Shared approximate token budget for requested target, callee, and \
                             type bodies (default {CONTEXT_BODY_TOKENS})"
                        ),
                    ),
                ],
            ),
        ),
        ToolDefinition::read_only(
            "get_impact",
            "Change-impact analysis at file:line[:column]: reference counts split by \
                          test vs prod, affected files, and a transitive caller graph with \
                          explicit traversal limits (`blast_radius`). `is_exported` appears only for \
                          languages whose compiler decides visibility, and is absent \
                          elsewhere rather than read off a naming convention. Use depth=1 \
                          for quick surveys, a greater depth when tracing wider caller reach.",
            with_extra(
                location_schema(),
                &[
                    ("limit", "integer", "Maximum affected files to list"),
                    (
                        "depth",
                        "integer",
                        &format!(
                            "Transitive caller depth, 1-{CALL_GRAPH_MAX_DEPTH} (default \
                             {IMPACT_DEFAULT_DEPTH})"
                        ),
                    ),
                ],
            ),
        ),
        ToolDefinition::read_only(
            "get_diff_impact",
            "Analyze tracked working-tree changes against one revision (default HEAD), or staged changes. Reports changed symbols, static references, and unmeasured files. Check incomplete and unmeasured_files before interpreting totals; test references are not execution coverage.",
            schema_object(&[
                ("revision", "string", "One base revision, not a revision range; default HEAD"),
                ("staged", "boolean", "Compare staged changes instead of the working tree"),
                ("callers", "boolean", "Include caller analysis"),
                (
                    "max_symbols",
                    "integer",
                    &format!(
                        "Maximum changed symbols to analyze; default {DIFF_IMPACT_MAX_SYMBOLS}, \
                         0 means unlimited"
                    ),
                ),
            ], &[]),
        ),
        ToolDefinition::read_only(
            "get_diagnostics",
            "LSP diagnostics (errors, warnings, hints) for one file. The \
                          verification step after an editing tool writes a file. A `status` \
                          field of unconfirmed or unsupported means the list is not \
                          authoritative — empty then means unknown, not clean. When the list \
                          floods, narrow it with severity (e.g. \"error\") instead of \
                          reading every entry.",
            schema_object(
                &[
                    ("file", "string", "Project-relative file path"),
                    (
                        "severity",
                        "string",
                        "Optional severity filter, comma-separated: error, warning, info, hint",
                    ),
                    (
                        "source",
                        "string",
                        "Optional diagnostic source filter (e.g. rust-analyzer)",
                    ),
                    (
                        "with_context",
                        "boolean",
                        "Attach each diagnostic's related definition/type-definition in one \
                         call (default false; multiplies LSP round-trips — batch only when \
                         fixing a multi-error file)",
                    ),
                    (
                        "with_suggestions",
                        "boolean",
                        "Attach LSP quickfix code actions per diagnostic (default false)",
                    ),
                ],
                &["file"],
            ),
        )
        .with_output_schema(with_extra(
            section_output_schema(
                "Diagnostics: severity, message, line, column, end_line, end_column, \
                 code?, source?, tags?, context? (with_context), suggestions? + \
                 suggestions_status? (with_suggestions; status 'unavailable' = the \
                 quickfix lookup failed, so empty means unknown)",
            ),
            &[
                (
                    "file",
                    "string",
                    "Project-relative file the diagnostics were pulled for",
                ),
                (
                    "status",
                    "string",
                    "Present only when the list is not authoritative: unconfirmed \
                     (no confirmed analysis within the wait window) or unsupported \
                     (the server provides no diagnostics) — empty items then \
                     means unknown, not clean",
                ),
            ],
        )),
        ToolDefinition::read_only(
            "build_context_pack",
            "Build a token-budgeted context pack: files ranked from syntax-level import paths \
                          with top-level signatures fitted to a token budget. This is an \
                          orientation aid, not a complete dependency graph. Module strings requiring \
                          unsupported decoding and Rust paths whose file the crate's module tree \
                          leaves open are omitted. Set focus for a \
                          known task area. Set shape=\"markdown\" for a \
                          plain-text view ready to paste into an LLM context window.",
            schema_object(
                &[
                    (
                        "tokens",
                        "integer",
                        &format!("Approximate token budget (default {PACK_TOKENS})"),
                    ),
                    (
                        "focus",
                        "string",
                        "Optional file path or substring to bias the ranking",
                    ),
                    (
                        "per_file",
                        "integer",
                        &format!("Cap on top-level symbols per file (default {PACK_SYMBOLS_PER_FILE})"),
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
                          without writing. Application requires confirmed workspace readiness \
                          unless accept_unconfirmed is set after reviewing a preview. \
                          ⚠ Mutates source files when dry_run is false.",
            with_required(
                with_extra(
                    position_exact_location_schema(),
                    &[
                        ("new_name", "string", "Replacement identifier"),
                        (
                            "dry_run",
                            "boolean",
                            "Preview the rename without writing (default false)",
                        ),
                        (
                            "accept_unconfirmed",
                            "boolean",
                            "Apply although the server cannot confirm workspace readiness \
                             (indexing: unconfirmed); review a dry_run first. A timed-out \
                             readiness wait is still refused (default false)",
                        ),
                    ],
                ),
                &["new_name"],
            ),
        ),
        ToolDefinition::read_only(
            "list_code_actions",
            "List LSP code actions available at file:line:column \
                          (refactor/quickfix/source). Filter with `kind` or `preferred=true`.",
            with_extra(
                position_exact_location_schema(),
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
            with_required(
                with_extra(
                    position_exact_location_schema(),
                    &[
                        ("title", "string", "Exact action title to apply"),
                        (
                            "dry_run",
                            "boolean",
                            "Preview the change without writing (default false)",
                        ),
                    ],
                ),
                &["title"],
            ),
        ),
        ToolDefinition::mutating(
            "replace_symbol_body",
            "Replace the symbol's entire definition — the span covered by the body \
                          search_symbols or list_file_symbols returns: any decorator, attribute \
                          or doc-comment lines it opens with, its signature, braces and body. \
                          Pass the complete definition as body, not just the inner code; a body \
                          read there, changed and passed back keeps those lines. Target by file + symbol path \
                          (e.g. 'Class/method') or by file:line[:column] — exactly one of \
                          symbol or line. \
                          ⚠ Mutates source files when dry_run is false.",
            with_required(
                with_extra(
                    edit_target_schema(),
                    &[
                        (
                            "body",
                            "string",
                            "Complete replacement for the symbol's definition \
                             (signature + braces + body)",
                        ),
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
                        (
                            "verify_callers",
                            "boolean",
                            "Also attach diagnostics for the edited symbol's caller files, its \
                             one-hop references (default false)",
                        ),
                    ],
                ),
                &["body"],
            ),
        ),
        ToolDefinition::mutating(
            "insert_before_symbol",
            "Insert source code immediately before the symbol, targeted by \
                          file + symbol path or file:line[:column] — exactly one of symbol or \
                          line. ⚠ Mutates source files when dry_run is false.",
            with_required(
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
                        (
                            "verify_callers",
                            "boolean",
                            "Also attach diagnostics for the edited symbol's caller files, its \
                             one-hop references (default false)",
                        ),
                    ],
                ),
                &["code"],
            ),
        ),
        ToolDefinition::mutating(
            "insert_after_symbol",
            "Insert source code immediately after the symbol, targeted by \
                          file + symbol path or file:line[:column] — exactly one of symbol or \
                          line. ⚠ Mutates source files when dry_run is false.",
            with_required(
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
                        (
                            "verify_callers",
                            "boolean",
                            "Also attach diagnostics for the edited symbol's caller files, its \
                             one-hop references (default false)",
                        ),
                    ],
                ),
                &["code"],
            ),
        ),
        ToolDefinition::mutating(
            "delete_symbol",
            "Delete the symbol's full definition. Target by file + symbol \
                          path (e.g. 'Class/method') or by file:line[:column] — exactly one of \
                          symbol or line. Always reports references outside the deleted span \
                          that would dangle; set expect_no_references=true to refuse the \
                          delete unless verified reference-free (fail-closed when \
                          unverifiable). Set dry_run=true to preview. ⚠ Mutates source files \
                          when dry_run is false.",
            with_extra(
                edit_target_schema(),
                &[
                    (
                        "expect_no_references",
                        "boolean",
                        "Refuse the delete unless verified reference-free; refusals \
                         return a precondition_failed error (default false)",
                    ),
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
