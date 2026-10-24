# CLI command layer

Commands emit typed values through `OutputContext`; CLI and MCP share those commands. The JSON contract is in `.claude/rules/json-output-stability.md`.

Suggested commands encode dynamic arguments with `utils::shell_arg`. Place options before `--` and positional values after it; use `--flag=value` for dynamic option values.

## Results and discovery

Use `Section<T>` for enumerations. Shape lower bounds, coverage, currency, hints, and remedies through `response::disclosure`. These are distinct facts: unread input bounds the consulted sources; missing languages qualify coverage; stale rows and tree currency describe index freshness. Derive each from the source outcome rather than asserting it at a call site. Keep causes with their remedies when capping advice.

Source-bearing responses use `OutputContext::print_source` with explicitly enumerated body paths. It omits whole bodies before dropping list items and never edits arbitrary nested data. Preserve declaration identity and signature; `body_omitted` names the budget. Context budgets target, callees, then types together. Recount `bodies_included` after response fitting.

Every search resolves `--path` through `search::common::resolve_search_paths`, confines each of its sources and the qualifiers they raise before counting, and carries the same paths into the follow-ups it advises. The resolver refuses a path the project's ignore rules exclude, so no search answers about one; symbol search also refuses a hidden one, since the index holds nothing under it.

`symbol_discovery` owns ranking, language detection, and steering. Textual exactness precedes path suffix, prefix, and substring matches; test scope and kind only break ties within those tiers. Do not infer query intent from ASCII word length or identifier suffixes. The counterpart and related files of `map file` are path suggestions, not semantic dependency evidence.

## Locations and references

Use `analysis::resolve_anchor` for symbol-level analysis and the shared `utils::symbol_nav` rules for declaration edits.

- Without a column, select the declaration on the line, otherwise the innermost enclosing declaration, including its decorators/attributes/docs.
- With a column, select a name span or resolve that exact token through LSP definitions. A header keyword/whitespace may address its declaration when no definition resolves; an empty body position addresses nothing. Never select an enclosing declaration for an unrelated body token.
- Match returned definitions to declaration name spans. Locals, parameters, module/file positions, and positions outside name spans are bindings rather than invented symbols. Preserve the existing self-definition handling and disclosed residuals for servers without precise name spans.
- `declared_in` supplies computed symbol paths. Keep exact, anchored-suffix, leaf, and wildcard matching consistent for nested LSP trees and flat AST declarations: read every pattern through `PathPattern`, whose `[kind]` qualifier selects that kind on every source, index rows included, and leave a declaration without a name to its position.

`LocationAnalysis` owns project-local references excluding the resolved declaration. Refs, context, and impact project from that same set. Per-position test classification belongs to `TestScope`; whole-file classification is for file summaries and ranking. Outside-project declarations still receive this project's inbound references and the matching disclosure.

Incoming/outgoing traversals share `call_graph::walk`, including deterministic frontier order, visited sets, depth/fan-out limits, and failed-hop markers. `diff-impact` assigns changed lines through `symbol_nav::members_at_line`, and reads deleted declarations from the base revision.

## Mutations and execution

All source writes go through `commands/edit/`: project-boundary validation, revision checks, previews, and typed output. Range batches share validated original-document byte offsets for application and metadata. Resolve CLI stdin before acquiring the write lock; MCP content is literal, including `-`.

LSP edits from rename and code actions use `apply_text_edits` / `apply_workspace_edits`. Source writes use `write_file` with observed content and retain the edit lock through resolution/application. Preserve revision hashes and check them again before replacement. `atomic_write` without a source precondition is for host configuration. Dry runs hold no write lock; multi-file application preflights and reports earlier applied files on failure.

Unsupported capabilities produce structured errors or explicitly qualified alternatives, never synthetic semantic results.

Extend existing commands rather than adding parallel surfaces. Agent-facing options must stay aligned with the curated MCP catalog and handlers; lifecycle/config/doctor remain CLI-only. Remove obsolete paths cleanly.
