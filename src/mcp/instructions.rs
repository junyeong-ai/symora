//! Server-level usage playbook, sent in the `initialize` result. MCP
//! hosts inject it into the model's context, which makes it the single
//! highest-leverage surface for teaching tool selection.
//!
//! Style contract: backticks are reserved for tool names. A test below
//! asserts every backtick-quoted token exists in the catalog, so a tool
//! rename cannot leave this playbook stale.

pub const INSTRUCTIONS: &str = "\
Symora answers code questions from AST + LSP analysis: exact symbols, \
references, and change impact. Prefer its targeted queries over reading \
whole files — they are cheaper and deterministic.

Exploration flow:
1. Orient: `build_context_pack` (token-budgeted repo brief) or `get_project_overview`. `check_language_support` says which languages this workspace can be answered for and by what — branch on serves before planning a session around a language, and read symbol_extraction / ast_search for what still works with no server.
2. Locate: `search_symbols` when you only know a rough name; `search_content` for keywords and strings. Both answer from the index: `get_index_status` says what a completed build covers, and an empty languages there means no build has run, so a zero proves nothing until `build_index` has. `search_ast` is the structural route for a language the index and the servers do not cover, and for forms a name cannot express.
3. Inspect: `get_file_overview` before reading any file in full; `list_file_symbols` for one file's exact symbol tree; `inspect_symbol` to resolve a path like Class/method from a search hit.
4. Navigate from a symbol position. The graph queries — `find_references`, `find_callers`, `find_callees`, `find_implementations`, `find_call_path` — take file:line[:column]: omit the column to address the symbol declared on the line, or give one to address the token there exactly as `find_definition` reads it — a call site resolves to the symbol called; `find_call_path` answers \"how does function A reach function B\" with the ordered call chain, or an honest reachability verdict that never overclaims. `find_definition` and `get_hover` resolve at the exact file:line:column — pass the symbol's column.
5. Aggregate: `get_context` returns callers, callees, types, and tests in one call — prefer it over four separate queries. For flow questions, set with_bodies=true: complete target, callee, and type bodies arrive in the same response — the target's whole body unbudgeted, callee and type bodies under a body_tokens budget; bodies_included on a section says how many items carry one — fetch any missing body with `inspect_symbol` instead of reading the file.
6. Before changing a symbol: `get_impact` reports test/prod reference counts, affected files, and a risk-ranked caller graph.

Editing tools — `rename_symbol`, `apply_code_action`, `replace_symbol_body`, `insert_before_symbol`, `insert_after_symbol`, `delete_symbol` — write source files. `replace_symbol_body` replaces the symbol's ENTIRE definition span — signature, braces, and body — so pass the complete definition, never just the inner code. `replace_symbol_body`, `insert_before_symbol`, `insert_after_symbol`, and `delete_symbol` take file plus exactly one of symbol or line: prefer symbol (a path like Class/method, exactly as returned by `search_symbols` or `list_file_symbols`) — it re-resolves against the live file, so sequential edits in one file don't invalidate each other the way line numbers do. `rename_symbol` and `apply_code_action` take file:line:column. Preview with dry_run=true first; the preview is an exact diff. `delete_symbol` additionally reports references that would dangle; set expect_no_references=true to refuse the delete unless the symbol is verified reference-free. After an edit lands, run `get_diagnostics` on the touched file (or set with_diagnostics=true on the edit itself) and fix what it reports; a status of unconfirmed or unsupported means the list is not authoritative — empty then means unknown, not clean.

Conventions: positions are 1-indexed lines and columns. An answer carrying indexing was computed while the language server was still reading the workspace: a list under it is a lower bound, and a single answer — `find_definition`, `get_hover` — that came back empty under it means not settled yet rather than nothing there. Retry once it clears. `rename_symbol` refuses to apply under it and previews instead, because a partial edit set renames some call sites and leaves the rest. List responses share one shape (count, showing, items, truncated, hints, next_commands). Errors carry {code, message, hint}; the hint names a working alternative. A conflict code from an editing tool means the file changed since it was analyzed — re-read it and retry. A precondition_failed code means an asserted precondition (e.g. expect_no_references) is unmet or could not be verified — re-reading and retrying will not clear it; follow the hint: fix the listed references, wait out indexing, or drop the assertion. A response that would exceed the size ceiling has whole items dropped from its largest list: count stays the true total, truncated is set, and a hint names the remedy.

Anti-patterns: don't address an edit by line when you hold a symbol path — line numbers go stale after every edit; don't guess a position — take it from a search or list result; don't read full file bodies to find one symbol; don't retry a capability the server reported as unsupported — follow the error's hint instead.";

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
