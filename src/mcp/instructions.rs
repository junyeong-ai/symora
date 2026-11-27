use super::McpProfile;

const COMMON: &str = "\
Symora provides code discovery, semantic navigation, bounded static impact analysis, and source inspection within the project root.

Choose the narrowest useful query. Use `search_symbols` for names, `search_content` for strings, and `search_ast` for syntax. For a known file, use `get_file_overview` or `list_file_symbols`, whose refs option counts each listed declaration's references in one call. A symbol path is a bare name, a Class/method suffix, a */method wildcard, or an exact name_path; a trailing [kind] selects one kind among same-named declarations (Svc[class], not its constructors; Circle[impl], its impl blocks). Use `get_diff_impact` to analyze tracked changes against one revision.

Search skips ignored, oversized and hidden files; content and AST search read hidden files inside a path they are given. A zero settles absence only within the searched sources and scope. Index rows are snapshots (`get_index_status` reports their coverage): stale and currency hints require refresh before current enumeration. A qualifier is present only when set, so read an absent one as false. Incomplete is the one marker that says a count is a lower bound because of the command's own caps or failures, including a call-graph walk's max_depth_reached, callers_truncated or callees_truncated, and failed_hops; coverage_gaps and indexing qualify counts too, and truncated means found items were omitted while the count stays exact: narrow the query or follow its hint naming the cap.

Use the find_* tools, `get_hover`, `get_context`, and `get_impact` for semantic follow-up. Positions are 1-indexed Unicode-scalar columns. Every result states its own position flat as file, line, and column, the name start to pass back; a span adds end_line (inclusive) and end_column (exclusive) where the source states them, and a related position (call_site, definition, target) is a nested object of the same shape. Read the echoed target: anchor_status is absent for a resolved declaration, binding for one the symbol tree omits (a local, a parameter), and not_a_symbol or unavailable when nothing resolved, so the results are about no declaration.Static graphs and test-reference ratios do not prove runtime reachability or change safety; reflection, structural Protocol conformance, registration, serialization, and external consumers need additional inspection. Callers marked references_derived were read off references, not verified call edges. Indexing unconfirmed means workspace readiness was not confirmed throughout the query; only a server that reports workspace status (rust-analyzer) confirms it, so elsewhere retrying does not clear it. Indexing timed_out means the readiness wait ended before the server reported status; retry once it is warm.

Requested bodies are complete or omitted; body_omitted names token_budget or response_size: inspect source at the returned location or adjust that budget. A diagnostics list whose status is unconfirmed, unsupported, or unavailable is unknown rather than clean; run the project's checks before taking a clean list as evidence.

Errors carry code, message, and hint. On conflict, wait as directed and re-read. On cancelled with content-change hints, resolve the target again. Repair precondition_failed and config_errors before depending on the result; choose another source on unsupported. On server_not_installed or lsp_unavailable, call `check_language_support` and read serves, not only installed; installing or configuring a server is the user's step. Verify degraded_column before editing. Hints and next_commands are CLI command lines: call the tool backed by the same command (symora refs is `find_references`), and ask the user to run one that no tool backs.";

const MUTATIONS: &str = "\
`build_index` refreshes the index incrementally. Mutating tools are `replace_symbol_body`, `insert_before_symbol`, `insert_after_symbol`, `delete_symbol`, `rename_symbol`, and `apply_code_action`. Preview using dry_run and check target_symbol, lines and preview. Prefer symbol paths from results over stale line numbers. Verify edits with diagnostics and project checks.";

pub fn for_profile(profile: McpProfile) -> String {
    let specific = match profile {
        McpProfile::Full => MUTATIONS,
        McpProfile::ReadOnly => {
            "This profile provides read-only tools. Ask for an index refresh through the CLI or a full-profile session when needed; use available AST/live inspection for current facts."
        }
    };
    format!("{COMMON}\n\n{specific}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_reference_only_tools_visible_in_the_profile() {
        for profile in [McpProfile::Full, McpProfile::ReadOnly] {
            let catalog = crate::mcp::visible_catalog(profile);
            let instructions = for_profile(profile);
            let mut rest = instructions.as_str();
            let mut seen = 0;
            while let Some(start) = rest.find('`') {
                let after = &rest[start + 1..];
                let end = after.find('`').expect("balanced tool quotes");
                let name = &after[..end];
                assert!(
                    catalog.iter().any(|tool| tool.name == name),
                    "{profile:?}: {name}"
                );
                seen += 1;
                rest = &after[end + 1..];
            }
            assert!(seen >= 10);
        }
    }

    /// Claude Code shows a server's instructions only up to 4,096 characters
    /// and puts a truncation marker in place of the rest, so whatever the
    /// playbook says past that never reaches the agent. A detail that concerns
    /// one tool belongs in that tool's description, which loads with the tool.
    /// Bytes bound the count in characters and UTF-16 units alike.
    #[test]
    fn instructions_fit_the_host_instruction_limit() {
        for profile in [McpProfile::Full, McpProfile::ReadOnly] {
            let length = for_profile(profile).len();
            assert!(length <= 4096, "{profile:?}: {length} bytes");
        }
    }
}
