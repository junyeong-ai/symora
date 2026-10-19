---
name: symora
metadata:
  version: 0.31.4
description: Provides symbol discovery, semantic navigation, static impact analysis, and symbol-aware edits through the symora CLI. Use when locating code, tracing references or callers, reviewing changes, or editing a known declaration.
allowed-tools: Bash(symora *)
---

# Symora

Choose the narrowest source that answers the task. A known file or declaration does not require a repository-wide map, pack, or index build. Run a command with `--help` for its detailed options.

## Find and inspect

```bash
symora search symbols AuthService --lang python --limit 5
symora symbols src/auth.py --symbol 'AuthService/login' --signature
symora refs src/auth.py:42
symora context src/auth.py:42 --with-bodies --body-tokens 2000
symora diff-impact
```

- Use `search symbols` for names, `search ast` for syntax, and `search content` for strings. Content search defaults to code. Name a format with `--lang markdown`, or select files/directories with `--path .claude` to search their text live, including hidden paths. Project ignores and size limits still apply; `.git` and `.symora` remain excluded.
- `symbols` and `map file` use the language server when available, otherwise AST declarations; `backend` identifies the producer. `--deterministic` permits only index/grammar sources and returns `unsupported` for semantic operations.
- For repeated workspace queries, inspect `search index status` and build incrementally with `search index build` when coverage is absent or the response requests refresh. Wait for an existing build. `--force` rebuilds everything.
- `map summary` and `pack --tokens 4000` are optional orientation tools. `map related` suggests names/directories. Pack uses syntax-derived import paths, not a complete dependency graph or task-relevance model. Rust relative module paths require unresolved crate/module ownership and are omitted with a hint. Use `--focus` for a known area; an unmatched focus is refused. Check coverage hints and `declared_symbols` before treating excerpts as complete.
- `diff-impact [REV]` compares tracked working-tree changes with one revision, not a revision range; `--staged` uses the index. Read `unmeasured_files` and `incomplete`.

## Address the intended symbol

Lines and Unicode-scalar columns are 1-indexed.

- For `refs`, callers/callees, implementations, type hierarchy, context, impact, and location-based usage: `file:line` addresses the declaration on that line or its enclosing symbol. An explicit column addresses the exact token and resolves usages through their definitions. Header keywords/whitespace may address that declaration; empty body positions address nothing.
- `def`, hover, signature, typedef, and rename are position-exact; an omitted column means 1. Take name positions from actual results.
- Read the echoed target and `anchor_status`. A binding is a local or parameter absent from the document-symbol tree. Source edits address declarations, not body tokens.

## Interpret limits

Lists carry `count`, `showing`, and `items`. These markers qualify the answer:

| Marker | Meaning and next action |
|---|---|
| `truncated` | Found items were omitted; narrow the query or follow the limit hint. |
| `incomplete` / `unread_paths` | Counts are lower bounds within the consulted sources; inspect the cause. |
| `coverage_gaps` | Named languages were not fully searched; use the indicated source or remedy. |
| `stale` / currency hints | The index is a snapshot; refresh before current enumeration. New files can require refresh even without stale returned rows. |
| `indexing` | `unconfirmed` lacks an explicit completion signal; `timed_out` exceeded the wait. Neither proves absence. Retry cannot add a missing completion channel. |
| `degraded_column` | Verify the source position before editing. |
| `body_omitted` | `token_budget` or `response_size`: read the complete source at the returned location, or adjust that budget. |

Requested bodies are complete or omitted. Context shares `--body-tokens` across target, callees, then types; `bodies_included` counts emitted section bodies. Missing bodies may also be unresolvable or absent in source. Token estimates are character-based, not model-specific. `output.max_response_chars` can remove bodies before list items; 0 disables that size target.

Static references and caller graphs do not prove runtime reachability, change safety, or executed test coverage. Inspect registration, reflection, structural Protocol implementations, serialization, and external contracts when relevant. Reference-derived callers are marked `callers_status: references_derived`; `--no-fallback` requires verified call hierarchy.

## Edit and verify

```bash
symora edit replace-body src/auth.py --symbol 'AuthService/login' --body - --dry-run < replacement.py
symora edit delete src/auth.py --symbol obsolete --expect-no-references --dry-run
symora rename src/auth.py:42:9 new_name --dry-run
symora diagnostics src/auth.py --with-context
```

- `replace-body` takes the entire definition, including signature and decorators. Obtain complete source before replacing it; an omitted body is not replacement text.
- Prefer a returned symbol path over line numbers for sequential edits. `insert-before`/`insert-after` take `--code`; range replace and AST pattern take `--text`. Range `--expect` checks the live text.
- Preview first. Applied edits can request `--with-diagnostics`; symbol replacement/insertion can also use `--verify-callers`. Unknown diagnostics are not clean; run relevant project checks.
- Delete's `--expect-no-references` requires verified static reference freedom. A static zero alone does not establish that code is unused.
- Rename can apply with `indexing: unconfirmed`; preview and validate usages. `timed_out` blocks application.
- Source edits resolve within the project and are checked against observed content under a shared lock. AST batch replacements require non-overlapping matches. Multi-file application is not atomic; inspect already-applied files before retrying an error.

## Recover

Failures emit `{ "error": { "code", "message", "hint" } }` on stdout and exit nonzero; logs use stderr.

- On `server_not_installed` / `lsp_unavailable`, run `doctor <lang>` and read `serves`, not only `installed`. A configured absolute server command avoids a broken version-manager shim; restart a warm daemon after config changes.
- On `unsupported`, choose an index/AST/source alternative. On `conflict`, wait as directed and re-read. On `cancelled` with a content-change hint, resolve the target again. Repair `precondition_failed` and `config_errors` before depending on the result.
- `lsp.timeout_secs` is the shared LSP work budget: startup, requests, and retries consume it together. Narrow expensive queries or explicitly configure a suitable budget; repeated calls do not resolve a missing capability.

Use `--format compact` for one-line JSON. `--check-version '<requirement>'` enforces a binary contract. `setup skill` installs the playbook bundled with that binary; `--git-ref` explicitly selects another source.
