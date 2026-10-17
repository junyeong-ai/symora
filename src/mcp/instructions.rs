//! Server-level usage playbook, exposed through MCP metadata. MCP
//! hosts inject it into the model's context, which makes it the single
//! highest-leverage surface for teaching tool selection.
//!
//! Style contract: backticks are reserved for tool names. A test below
//! asserts every backtick-quoted token exists in the catalog, so a tool
//! rename cannot leave this playbook stale.

pub const INSTRUCTIONS: &str = "\
Symora provides code discovery, semantic navigation, static impact analysis, and source edits within the server's project root.

Choose a query: `get_project_overview` or `build_context_pack` for orientation; `search_symbols` for names; `search_content` for strings; `search_ast` for syntax; `get_file_overview`, `list_file_symbols`, and `inspect_symbol` for a known file or declaration. `check_language_support` reports serving language servers and serverless grammar support. `get_index_status` reports completed index coverage; `build_index` refreshes it incrementally. Content search defaults to code files: name a documentation/data language explicitly. Discovery excludes hidden paths, ignored files, and files over the configured size limit.

Index rows describe an indexed snapshot. A stale marker concerns returned rows; a separate currency hint also covers new files with no rows yet. Rebuild before using an outdated index as a current enumeration. Counts are lower bounds when incomplete, coverage_gaps, or indexing qualifies them. Truncated means found items were omitted from the response. A zero establishes absence only within the searched sources and scope, never runtime unreachability.

Use `find_definition`, `get_hover`, `find_references`, `find_callers`, `find_callees`, `find_call_path`, `find_implementations`, `get_context`, and `get_impact` for semantic follow-up. Positions use 1-indexed lines and Unicode-scalar columns. For symbol-level analysis, omit column to address the declaration on the line or its enclosing symbol; supply column to address that exact token, resolving a usage through its definition. Read the echoed target and anchor_status. Static caller graphs report their bounds; they do not predict change safety. Reflection, structural Protocol conformance, string registration, external consumers, and serialized contracts can require additional inspection. The indexing marker unconfirmed means no explicit workspace completion signal; time or logs cannot establish readiness. Rename preserves this marker on server-provided edits; a timeout after explicit server activity blocks application. Reference-derived callers are marked callers_status: references_derived and are not verified call edges.

Mutating tools include `replace_symbol_body`, `insert_before_symbol`, `insert_after_symbol`, `delete_symbol`, `rename_symbol`, and `apply_code_action`. Preview with dry_run first. Symbol/range edits preview exact hunks; rename and code actions preview affected files and edit counts. Prefer a symbol path from a result over a line number for sequential edits. replace_symbol_body takes the entire definition, including signature, decorators/attributes, and body, as returned by inspect_symbol. Delete's expect_no_references requires verified static reference freedom. Edits are serialized per project and checked for observed content changes. A multi-file write is not an atomic transaction; an error names earlier applied files, which must be inspected before retrying.

Use `get_diagnostics` after editing, or request with_diagnostics on the edit. Unconfirmed, unsupported, or unavailable diagnostics are unknown, not clean. Project type checking and tests still verify behavior. get_context with_bodies attaches complete source: target unbudgeted, callees/types under body_tokens. Missing bodies can also be unresolvable or absent in source.

Errors use {code, message, hint}. A conflict means changed content or a held resource: wait as directed, re-read, then retry. A cancelled response with a content-change hint requires re-reading the source and resolving the target before requesting fresh edits; unchanged coordinates may address another symbol. A precondition_failed needs its condition repaired. An unsupported capability needs an alternative, not repeated calls. A degraded_column must be verified before use as an edit address. Whole list items may be dropped to meet the response size ceiling; follow its hint or narrow the query.";

#[cfg(test)]
mod tests {
    use super::INSTRUCTIONS;

    /// Backticks are reserved for tool names; every quoted token must be
    /// a real catalog entry. This is what keeps the playbook honest when
    /// tools are added, renamed, or removed.
    #[test]
    fn instructions_reference_only_real_tools() {
        let names: Vec<&str> = super::super::tools::catalog()
            .iter()
            .map(|t| t.name)
            .collect();
        let mut rest = INSTRUCTIONS;
        let mut seen = 0;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let end = after
                .find('`')
                .expect("unbalanced backtick in INSTRUCTIONS");
            let token = &after[..end];
            assert!(
                names.contains(&token),
                "INSTRUCTIONS references unknown tool `{token}`"
            );
            seen += 1;
            rest = &after[end + 1..];
        }
        assert!(seen > 10, "INSTRUCTIONS should reference the core tools");
    }
}
