use crate::constants::defaults::{
    CALL_GRAPH_MAX_DEPTH, CONTEXT_BODY_TOKENS, DIFF_IMPACT_MAX_SYMBOLS, IMPACT_DEFAULT_DEPTH,
    MAP_FILE_DEPTH, MAP_FILE_TESTS_LIMIT,
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
            "High-level project map: language breakdown, top directories, the packages \
                          its workspace manifests declare (Cargo, npm or yarn, pnpm, uv, go.work; \
                          absent where none declares a workspace), and the entrypoints its \
                          manifests declare (Cargo targets, Python scripts and __main__.py, \
                          package.json bin and main, Go main packages), each with a kind of bin, \
                          lib, script or main_module and its name where it has one — never a \
                          file merely named like one. hints name a manifest that did not parse \
                          and a declared entrypoint naming no file. Cheap orientation step \
                          before deeper queries.",
            schema_object(&[], &[]),
        ),
        ToolDefinition::read_only(
            "get_file_overview",
            "Compact map for one file: siblings, the tests that name it, and its \
                          shallow symbol tree. Each test carries its evidence: imports (its \
                          imports name the file), imports_package (a Go import path or JVM \
                          wildcard names its package), same_package (a Go _test.go file beside \
                          it) or test_region (lines of the file compiled only under test); a \
                          file sharing its name is not listed, the section's hint says what the \
                          reading leaves out, and a language it does not read is the section's \
                          unsupported error. Use \
                          find_references for semantic links and get_context with tests for the \
                          tests referencing one declaration. When nothing read the file's \
                          declarations, `backend` is omitted and `symbols` carries the error — \
                          an empty list would say the file declares none.",
            schema_object(
                &[
                    ("path", "string", "Project-relative file path"),
                    (
                        "depth",
                        "integer",
                        &format!("Nested-symbol depth (default {MAP_FILE_DEPTH})"),
                    ),
                    (
                        "tests_limit",
                        "integer",
                        &format!("Maximum tests to show (default {MAP_FILE_TESTS_LIMIT})"),
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
                          needs no second call. A language coverage_gaps lists was not covered, \
                          so a match's absence there proves nothing: search it with \
                          search_content or search_ast, or fix what its reason names. In a git \
                          work tree the first search builds the index in .symora/ and answers \
                          from it; elsewhere a hint says no index took part and build_index \
                          builds one.",
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
                            "Optional kind filter (function, class, struct, impl, ...); impl is a \
                             block that adds members to a type declared elsewhere (a Rust impl, a \
                             Swift or Dart extension), named for that type; the index and \
                             a server can name one declaration's kind differently, so a zero \
                             under it is unverified",
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
                    "description": "Project-relative files/directories whose declarations to search; \
                                    a hidden path is refused"
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
                          instead of discovering a language's limits by a failed call. serves \
                          confirms the server's handshake, not its import resolution or \
                          complete references. For Python, python_interpreter says which \
                          interpreter a fresh server receives (its source: settings, \
                          project_environment or server_default); where references across \
                          packages stay missing, that environment is what to check, and \
                          waiting does not prepare it.",
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
                "Per language: server, installed, serves, symbol_extraction, ast_search, \
                 python_interpreter for Python, and install when it does not serve. `serves` is \
                 the capability — `installed` only says a file resolves at that path, which a \
                 version-manager shim satisfies too",
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
            "Matches: file, line, column, end_line, end_column (inclusive line, exclusive \
             column), text, captures",
        )),
        ToolDefinition::read_only(
            "get_index_status",
            "Whether the symbol/content index is built and what it covers. An unbuilt or \
                          narrowed index changes what search_symbols and search_content can \
                          answer, so read this before treating a zero as absence. languages \
                          lists what the last completed build extracts symbols for, empty when \
                          none has completed (last_indexed 0), and a symbol search is complete \
                          only for those.",
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
            "Mutates the project's search index (.symora/), never source. Refresh it after \
                          edits so symbol and content search answer for the working tree, or \
                          rebuild or narrow it — the remedy a currency hint and a not_indexed \
                          coverage gap name. search_symbols builds a missing index on its own \
                          only in a git work tree; outside one, build it here.",
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
                         refs_status marking a lower bound (indexing_degraded) or why no count \
                         was taken (timed_out when the lsp.timeout_secs budget ran out, \
                         unsupported, unavailable)",
                    ),
                ],
                &["file"],
            ),
        )
        .with_output_schema(with_extra(
            section_output_schema(
                "Symbols defined in the file: name, name_path?, kind, file, line, column (the \
                 name), end_line?/end_column? (where the declaration ends), children?",
            ),
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
             treating the count as complete; unconfirmed does not verify workspace readiness. \
             A Python __init__ or __new__ counts only the references to the method itself, \
             since pyright reports Circle(…) as a reference to the class.",
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
                    "The symbol the position resolved to: { name, kind, file, line, column, signature?, \
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
            "Incoming-call hierarchy for a function at file:line[:column]. Without the \
             server's call hierarchy, callers are read off references (callers_status \
             references_derived), not verified call edges, and a type or value reads \
             not_callable and lists none. A Python __init__ or __new__ has no references of \
             its own, since pyright reports Circle(…) as a reference to the class: its callers \
             are read off the calls of its class and marked references_derived, a lower bound \
             its hint names.",
            with_extra(
                location_schema(),
                &[
                    ("limit", "integer", "Maximum results"),
                    (
                        "no_fallback",
                        "boolean",
                        "Refuse the references-derived approximation when the server lacks call \
                         hierarchy, returning unsupported instead (default false)",
                    ),
                ],
            ),
        )
        .with_output_schema(with_extra(
            section_output_schema(
                "Incoming calls: name, kind, file, line, column (the caller's declaration), \
                 call_site { file, line, column }",
            ),
            &[(
                "callers_status",
                "string",
                "Present only when callers were derived from references — because the \
                 server lacks call hierarchy, or a Python constructor's constructions, which \
                 the server reports as references to its class, joined its list — \
                 reference-based, not verified call edges; not_callable, with no items, for \
                 a type or value whose server lacks call hierarchy",
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
            section_output_schema(
                "Outgoing calls: name, kind, file, line, column (the callee's declaration), \
                 call_site?",
            ),
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
                    "failed_hops",
                    "boolean",
                    "A hop's callee query failed and was treated as empty, so the set is a \
                     lower bound (depth>1 only)",
                ),
                (
                    "incomplete",
                    "boolean",
                    "Present (true) only when count is a lower bound; with depth>1, set by \
                     max_depth_reached, callees_truncated or failed_hops, which name the cause",
                ),
                (
                    "anchor_status",
                    "string",
                    "Present when the from-position did not resolve to a listed symbol: \
                     \"binding\" (a declaration the symbol tree does not list, such as a local or \
                     a parameter; the set is exactly its), \
                     \"not_a_symbol\" (read OK, denotes nothing) or \"unavailable\" (a read \
                     failed, or the server was still loading its workspace); for the last two an \
                     empty set is not authoritative (depth>1 only)",
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
                          'unreachable'. target_status reads for `to` as anchor_status does for \
                          the anchor; not_a_symbol or unavailable there forces \
                          not_reached_within_bound, which is then not authoritative, and so does \
                          not_callable, marking an endpoint that is no function, method or \
                          constructor, to or from which no chain was found.",
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
                "failed_hops",
                "boolean",
                "A hop's call query failed and was treated as empty, so the \
                     verdict is a lower bound — never an exhaustive negative",
            ),
            (
                "incomplete",
                "boolean",
                "The search stopped short of the whole reachable graph: set by \
                     max_depth_reached, callees_truncated or failed_hops, which name the cause",
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
            "Aggregated symbol context: callers, callees, related types, and tests. all (the default) attaches the target's body; with_bodies=true adds the callees' and types' bodies, all sharing body_tokens in target/callee/type order. Bodies that do not fit are omitted with body_omitted; source-size fitting can also omit bodies. bodies_included counts emitted section bodies. Missing bodies can be unresolvable or absent in source. refs.is_exported appears only where the compiler decides visibility, so its absence is not false.",
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
                    ("tests", "boolean", "Include the tests whose code references the target"),
                    (
                        "all",
                        "boolean",
                        "Include every context section and the target's body under body_tokens (default true)",
                    ),
                    (
                        "with_bodies",
                        "boolean",
                        "Attach complete bodies under body_tokens (default false)",
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
        )
        .with_output_schema(output_schema(&[
            ("target", "object", "The symbol the position resolved to: { name, kind, file, line, column, \
                 signature?, body?, anchor_status? }; anchor_status is present only when the \
                 position did not resolve to a listed symbol"),
            ("refs", "object", "Reference summary: { count, test, prod, files, modules, is_exported?, \
                 indexing?, incomplete? }; incomplete marks count as a lower bound"),
            ("hints", "array", "What the reference summary does not settle"),
            (
                "callers",
                "object",
                "Incoming calls as a list (count, showing, items of name, kind, file, line, \
                 column, call_site)",
            ),
            (
                "callees",
                "object",
                "Outgoing calls as a list (count, showing, items of name, kind, file, line, \
                 column, call_site?, body?)",
            ),
            (
                "types",
                "object",
                "Related types as a list (items of name, kind, file, line, column, detail?, \
                 body?)",
            ),
            (
                "tests",
                "object",
                "Related tests as a list (items of name, file, line, column)",
            ),
        ])),
        ToolDefinition::read_only(
            "get_impact",
            "Change-impact analysis at file:line[:column]: reference counts split by \
                          test vs prod, affected files, and a transitive caller graph with \
                          explicit traversal limits (`blast_radius`). `is_exported` appears only for \
                          languages whose compiler decides visibility, and is absent \
                          elsewhere rather than read off a naming convention. Use depth=1 \
                          for quick surveys, a greater depth when tracing wider caller reach. \
                          blast_radius is absent when no call hierarchy could be started, which \
                          is not zero callers; its dynamic_dispatch, present for a trait or \
                          interface or one of its methods, means callers through \
                          implementations are left out of its counts.",
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
        )
        .with_output_schema(output_schema(&[
            ("target", "object", "The symbol the position resolved to: { name, kind, file, line, column, \
                 signature?, body?, anchor_status? }; anchor_status is present only when the \
                 position did not resolve to a listed symbol"),
            ("refs", "object", "Reference summary: { count, test, prod, files, modules, is_exported?, \
                 indexing?, incomplete? }; incomplete marks count as a lower bound"),
            (
                "coverage",
                "object",
                "Test references: { count, files }, static references rather than execution",
            ),
            (
                "blast_radius",
                "object",
                "Caller graph: { direct_callers, transitive_callers, depth, \
                 max_depth_reached?, callers_truncated?, indexing?, failed_hops?, incomplete?, \
                 dynamic_dispatch?, callers_by_depth, test_caller_ratio? }; incomplete marks \
                 the counts a lower bound, and the flags before it name the cause. Absent when \
                 no call hierarchy could be started",
            ),
            ("hints", "array", "What the counts do not settle"),
            ("next_commands", "array", "Follow-ups the disclosures above call for"),
            (
                "files",
                "array",
                "Affected files, most-referenced first: { file, is_test, refs }",
            ),
        ])),
        ToolDefinition::read_only(
            "get_diff_impact",
            "Analyze tracked working-tree changes against one revision (default HEAD), or staged changes; untracked files are in neither, and a working-tree run names them in hints. path confines the analysis to the changed files some git pathspecs match, and a rename when either of its paths does, while references and callers are still searched across the whole project: changed_files_count then counts the matched files and excluded_files_count the changed files left out, which nothing in the answer measures. When max_symbols, the lsp.timeout_secs budget or the response size cuts a large diff short, measure it a part at a time with path. Reports changed symbols, static references, and unmeasured files; total_references is absent when no row was measured. Check incomplete, unmeasured_files, excluded_files_count, and each row's refs_status and callers_status before interpreting totals: refs_status marks a lower bound (indexing_degraded) or why no count was taken (timed_out when the lsp.timeout_secs budget ran out, unsupported, unavailable), and refs_status_counts and callers_status_counts count the rows of each status, so a total is a lower bound by those rows alone; test references are not execution coverage. Documentation and configuration files (Markdown, YAML, TOML, JSON) are listed in unmeasured_files rather than read by key. changes is a list with count, showing and truncated, and with callers, a changed function's, method's or constructor's callers is one too, read off the references its count was taken from (callers_status references_derived, not verified calls): every callable whose code holds one, a use as a value counted like a call, an import outside a callable not at all; another kind's row reads not_callable, and call_hierarchy asks the server instead. callers is absent where callers_status says none was obtained. A deletion whose declarations were not identified has no name or kind, and its position is the first line it removed.",
            {
                let mut schema = schema_object(&[
                    ("revision", "string", "One base revision, not a revision range; default HEAD"),
                    ("staged", "boolean", "Compare staged changes instead of the working tree"),
                    (
                        "callers",
                        "boolean",
                        "List each changed function's, method's and constructor's callers, read off \
                         the references its count was taken from",
                    ),
                    (
                        "call_hierarchy",
                        "boolean",
                        "Ask every changed symbol's callers of the server's call hierarchy \
                         instead, which it requires, a second workspace search per symbol; implies \
                         callers",
                    ),
                    (
                        "max_symbols",
                        "integer",
                        &format!(
                            "Maximum added and modified symbols whose references are counted, \
                             and apart from them, deleted symbols listed, so a deletion never \
                             takes a counted symbol's place; the files it stops in or before are \
                             listed in unmeasured_files; default {DIFF_IMPACT_MAX_SYMBOLS}, \
                             0 means unlimited"
                        ),
                    ),
                ], &[]);
                schema["properties"]["path"] = serde_json::json!({
                    "type": "array", "items": {"type": "string"}, "minItems": 1,
                    "description": "Analyse only the changed files these git pathspecs match, \
                        project-relative: a file or directory, a glob such as *.py, or :!dir to \
                        leave one out; a rename is analysed when either of its paths matches. \
                        A pathspec matching no file tracked now or at the revision is refused"
                });
                schema
            },
        )
        .with_output_schema(output_schema(&[
            ("revision", "string", "The revision compared with"),
            (
                "changed_files_count",
                "integer",
                "Files the diff changed, those path matches where it is given",
            ),
            (
                "excluded_files_count",
                "integer",
                "With path, the changed files it does not match, which nothing here \
                 measures; 0 when it matches every one; absent without path",
            ),
            (
                "total_references",
                "integer",
                "Sum of the measured rows' refs; absent when no row was measured",
            ),
            (
                "refs_status_counts",
                "object",
                "Rows per refs_status, { status: rows }, over the rows whose count is a lower \
                 bound or was not taken, which make total_references and coverage lower \
                 bounds; absent when every count is authoritative",
            ),
            (
                "callers_status_counts",
                "object",
                "Rows per callers_status, { status: rows }; absent when every caller list \
                 asked for is authoritative, and without callers",
            ),
            (
                "coverage",
                "object",
                "{ with_tests, without_tests, ratio? } over the measured rows",
            ),
            (
                "unmeasured_files",
                "array",
                "Files whose changes could not be measured in full, documentation and \
                 configuration among them, and those max_symbols stopped in or before; hints \
                 name the causes",
            ),
            (
                "incomplete",
                "boolean",
                "Present (true) only when max_symbols stopped the analysis, leaving changed \
                 symbols unlisted, so every count is a lower bound",
            ),
            ("hints", "array", "Causes and remedies"),
            (
                "changes",
                "object",
                "Changed symbols as a list (count, showing, truncated?, items) of { name?, \
                 kind?, file, line, column, change_type, refs?, test_refs?, prod_refs?, \
                 refs_status?, callers_status?, deletion?, callers? }; callers is a list of the \
                 same shape of { name, kind, file, line, column, call_site }",
            ),
        ])),
        ToolDefinition::read_only(
            "get_diagnostics",
            "LSP diagnostics (errors, warnings, hints) for one file. The \
                          verification step after an editing tool writes a file. A `status` \
                          field of unconfirmed or unsupported means the list is not \
                          authoritative — empty then means unknown, not clean. A file an edit \
                          did not change, such as a caller under verify_callers, can show its \
                          list from before the edit, and no response says whether it does: \
                          pyright and rust-analyzer compute the list when it is read, after \
                          edits made through these tools but possibly before they hear of \
                          another tool's; other servers report a caller's list only once they \
                          check it again. rust-analyzer's list is its own analysis: \
                          use-after-move, lifetime errors and clippy lints come only from cargo \
                          check or cargo clippy. When the list floods, narrow it with severity \
                          (e.g. \"error\") instead of reading every entry.",
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
                        "Attach LSP quickfix code actions per diagnostic; suggestions_status \
                         unavailable means the lookup failed, so none listed is unknown \
                         (default false)",
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
        ToolDefinition::mutating(
            "rename_symbol",
            "LSP rename for the symbol at file:line:column. Returns the affected \
                          file list and per-file edit count. Application requires confirmed \
                          workspace readiness. Under indexing unconfirmed, compare each file's \
                          edit_count in a dry_run with find_references for the target, which \
                          leaves out the declaration the rename also edits, then apply with \
                          accept_unconfirmed; the response keeps the marker, so check that no \
                          reference kept the old name. timed_out permits previews only. Every \
                          file is checked before any is written, but a write that fails \
                          partway leaves earlier files modified; the error's hint names them, \
                          so inspect those before retrying. \
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
                            "Apply under indexing: unconfirmed, after the dry_run comparison \
                             the description gives; timed_out is still refused (default false)",
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
                          `list_code_actions` first to discover titles. Every file is checked \
                          before any is written, but a write that fails partway leaves earlier \
                          files modified; the error's hint names them, so inspect those before \
                          retrying. ⚠ Mutates source files when dry_run is false.",
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
                             one-hop references, whose lists can predate the edit (default false)",
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
                             one-hop references, whose lists can predate the edit (default false)",
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
                             one-hop references, whose lists can predate the edit (default false)",
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
                          symbol or line. Reports the references outside the deleted span that \
                          would dangle as dangling_references, at their positions in the file \
                          the call leaves, count 0 included, whenever the \
                          check ran; otherwise references_status (unsupported, unavailable) \
                          says why, and they are unknown. Set expect_no_references=true to \
                          refuse the \
                          delete unless verified reference-free (fail-closed when \
                          unverifiable). ⚠ Mutates source files when dry_run is false.",
            with_extra(
                edit_target_schema(),
                &[
                    (
                        "expect_no_references",
                        "boolean",
                        "Refuse the delete unless verified reference-free; refusals \
                         return a precondition_failed error. Under indexing unconfirmed it \
                         always refuses: check find_references yourself and delete without it \
                         (default false)",
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
