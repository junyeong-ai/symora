use super::McpProfile;

const COMMON: &str = "\
Symora provides code discovery, semantic navigation, bounded static impact analysis, and source inspection within the project root.

Choose the narrowest useful query. Use `search_symbols` for names, `search_content` for strings, and `search_ast` for syntax. For a known file, use `get_file_overview` or `list_file_symbols`, whose refs option counts each listed declaration's references in one call; for a known declaration in an unknown file, `search_symbols` with body or signature. Use `get_project_overview` for orientation; `build_context_pack` ranks syntax-derived import paths, not complete dependencies or task relevance. Use `get_diff_impact` to analyze tracked changes against one revision. `check_language_support` reports live-server and grammar support; `get_index_status` reports completed index coverage.

Search defaults to code and excludes hidden/ignored/oversized paths. Name a documentation/data language when needed, and pass path to confine a symbol, content, or AST search. A zero settles absence only within the searched sources and scope. Index rows are snapshots: stale and currency hints require refresh before current enumeration. Incomplete, coverage_gaps, and indexing qualify counts; truncated means found items were omitted.

Use `find_definition`, `get_hover`, `find_references`, `find_callers`, `find_callees`, `find_call_path`, `find_implementations`, `get_context`, and `get_impact` for semantic follow-up. Positions are 1-indexed Unicode-scalar columns. On symbol-level tools, omit column to address the declaration or enclosing symbol; supply column for the exact token, resolving a usage through its definition. Read target and anchor_status. Static graphs and test-reference ratios do not prove runtime reachability or change safety. Reflection, structural Protocol conformance, registration, serialization, and external consumers need additional inspection. Indexing unconfirmed means workspace readiness was not confirmed throughout the query. Only a server that reports workspace status (rust-analyzer) confirms it, so other servers' answers stay unconfirmed and retrying does not clear it. Indexing timed_out means the readiness wait ended before the server reported status; retry once it is warm. Reference-derived callers are not verified call edges.

Requested bodies are complete or omitted. get_context shares body_tokens across target, callees, and types. body_omitted names token_budget or response_size; inspect source at the returned location or adjust the relevant budget. Use `get_diagnostics` to inspect diagnostics; unconfirmed, unsupported, and unavailable are unknown rather than clean. Project checks still verify behavior.

Errors carry code, message, and hint. On conflict, wait as directed and re-read. On cancelled with content-change hints, resolve the target again. Repair precondition_failed; choose another source on unsupported. Verify degraded_column before editing. Narrow oversized lists or follow their size-limit hint.";

const MUTATIONS: &str = "\
`build_index` refreshes the index incrementally. Mutating tools are `replace_symbol_body`, `insert_before_symbol`, `insert_after_symbol`, `delete_symbol`, `rename_symbol`, and `apply_code_action`. Preview using dry_run. Prefer symbol paths from results over stale line numbers. replace_symbol_body takes the entire definition, including signature and decorators. Delete's expect_no_references requires verified static reference freedom. Edits are serialized and checked against observed content. Multi-file application is not atomic: inspect files already applied before retrying. Rename applies under confirmed workspace readiness. Under unconfirmed, review a dry_run and apply with accept_unconfirmed; timed_out permits previews only. Verify edits with diagnostics and project checks.";

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
}
