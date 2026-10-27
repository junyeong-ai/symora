---
name: symora
metadata:
  version: 0.35.0
description: Provides symbol discovery, semantic navigation, static impact analysis, and symbol-aware edits through the symora CLI. Use when locating code, tracing references or callers, reviewing changes, or editing or renaming a known declaration.
allowed-tools: Bash(symora search *), Bash(symora symbols *), Bash(symora map *), Bash(symora pack *), Bash(symora def *), Bash(symora refs *), Bash(symora typedef *), Bash(symora implementations *), Bash(symora callers *), Bash(symora callees *), Bash(symora supertypes *), Bash(symora subtypes *), Bash(symora hover *), Bash(symora context *), Bash(symora impact *), Bash(symora diff-impact *), Bash(symora diagnostics *), Bash(symora edit *), Bash(symora rename *), Bash(symora actions *), Bash(symora doctor *), Bash(symora daemon restart --project*)
---

# Symora

Choose the narrowest source that answers the task. A known file or declaration does not require a repository-wide map, pack, or index build. Run a command with `--help` for its detailed options.

## Find and inspect

```bash
symora search symbols AuthService --lang python --limit 5
symora symbols src/auth.py --symbol 'AuthService/login' --signature
symora refs src/auth.py:42
symora context src/auth.py:42 --callees --types --with-bodies --body-tokens 4000
symora diff-impact
```

- Use `search symbols` for names, `search ast` for syntax, and `search content` for strings, each confined to named files or directories with `--path`. `search symbols` answers a bare name from a built index when the index matches it; a name the index does not match, and a path (`Class/method`, `Name[kind]`), also ask language servers and can carry `indexing`. It takes `--body` or `--signature` to read a known declaration whose file is unknown. Content search defaults to code; name a format with `--lang markdown`. Searches skip ignored and oversized files; content and AST search read hidden files inside the paths they are given, while symbol search refuses a hidden path.
- `symbols` and `map file` use the language server when available, otherwise AST declarations, which also answer a `--symbol` path the server's tree lacks; `backend` identifies the producer. `symbols <file> --depth 1 --refs` counts references for each listed declaration, a type's members included and a function's parameters and locals excluded; `refs_status` marks a count that is unavailable or a lower bound. `--deterministic` never consults a language server: index, AST and file-scan sources answer, and a command that needs a server returns `unsupported`.
- For repeated workspace queries, inspect `search index status` and build incrementally with `search index build` when coverage is absent or the response requests refresh. Wait for an existing build. `--force` rebuilds everything.
- `map summary` and `pack --tokens 4000` are optional orientation tools. `map file` adds a file's name-matched counterparts, such as its tests, and nearby files; these are path suggestions, not dependency evidence. Pack ranks syntax-derived import paths, not a complete dependency graph or task-relevance model. A reference it cannot settle on one project file forms no edge, and its hints name what it could not read or resolve. Use `--focus` for a known area; an unmatched focus is refused. Check those hints and `declared_symbols` before treating excerpts as complete.
- `diff-impact [REV]` compares tracked working-tree changes with one revision, not a revision range; for a branch, pass `$(git merge-base main HEAD)`. `--staged` compares the git staging area instead of the working tree. Untracked files are in neither; a working-tree run names them in `hints`. Read `unmeasured_files` and `incomplete`.

## Address the intended symbol

Lines and Unicode-scalar columns are 1-indexed.

- For `refs`, callers/callees, implementations, type hierarchy, context, and impact: `file:line` addresses the declaration on that line or its enclosing symbol. An explicit column addresses the exact token and resolves usages through their definitions. Header keywords/whitespace may address that declaration; empty body positions address nothing.
- `def`, hover, typedef, and rename are position-exact; an omitted column means 1. Take name positions from actual results.
- A `--symbol` path is a bare name, a `Class/method` suffix, a `*/method` wildcard, or an exact `name_path`. A trailing `[kind]` selects one kind among same-named declarations (`Svc[class]`, not its constructors); a segment written wholly in brackets is a name (`Cache/[key]`). Kinds come from whichever source answered, and the index and a server can differ (pyright reports `MAX_SIZE` as `constant`, the index as `variable`). Take a kind from `symbols <file>` for the file you act on, and treat a zero under `--kind` as unverified.
- Read the echoed target. `anchor_status` is absent when a declaration resolved; `binding` is a declaration absent from the document-symbol tree, such as a local, a parameter, or one the server's outline omits; `not_a_symbol` and `unavailable` resolved nothing, so the counts are about no declaration. Source edits address declarations, not body tokens.

## Interpret limits

Lists carry `count`, `showing`, and `items`. These markers qualify the answer:

| Marker | Meaning and next action |
|---|---|
| `truncated` | Found items were omitted; narrow the query or follow the limit hint. |
| `incomplete` / `unread_paths` | Counts are lower bounds within the consulted sources; inspect the cause. |
| `coverage_gaps` | Named languages were not fully searched; use the indicated source or remedy. |
| `stale` / currency hints | The index is a snapshot; refresh before current enumeration. New files can require refresh even without stale returned rows. |
| `indexing` | `unconfirmed`: the server's workspace readiness was not confirmed throughout the query. Only a server that reports workspace status (rust-analyzer) confirms it, so other servers' answers carry `unconfirmed` and retrying does not clear it. `timed_out`: the readiness wait ended before the server reported status; retry once it is warm. Neither proves absence. |
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

1. Read the complete definition: `symbols <file> --symbol P --body`, or `search symbols P --body` when the file is unknown. `replace-body` takes the entire definition, signature and decorators included; an omitted body is not replacement text.
2. Run the edit with `--dry-run` and check `target_symbol`, `lines`, and `preview`.
3. Apply it. `replace-body`, `insert-before`/`insert-after`, `delete`, and range `replace` accept `--with-diagnostics`, and symbol replacement and insertion `--verify-callers`; after `edit pattern`, `rename` or `actions apply`, run `diagnostics`.
4. If diagnostics or the project's checks fail, fix and repeat from step 2. A diagnostics response that carries `status` (`unconfirmed`, `unsupported`, `unavailable`) is not clean, whatever its count; `status` is absent only from a confirmed list.

- Prefer a returned symbol path over line numbers for sequential edits. `insert-before`/`insert-after` take `--code`; range replace and AST pattern take `--text`. Range `--expect` checks the live text.
- Delete's `--expect-no-references` requires verified static reference freedom, so under `unconfirmed` it refuses: check `refs` yourself and delete without it. A static zero alone does not establish that code is unused.
- Rename applies under confirmed workspace readiness. Under `unconfirmed`, compare each file's `edit_count` in the `--dry-run` with `refs` for the target, which leaves out the declaration the rename also edits, then apply with `--accept-unconfirmed`; the response keeps the marker, so check that no reference kept the old name. `timed_out` permits previews only.
- `edit pattern` replaces match 0 unless `--index N` or `--index all` selects others; `--index all` refuses overlapping matches. Multi-file application is not atomic; inspect already-applied files before retrying an error.
- `actions list <file:line:col>` lists the server's code actions at a position; `actions apply <file:line:col> '<title>' --dry-run` previews one by its exact title.

## Recover

Failures emit `{ "error": { "code", "message", "hint" } }` on stdout and exit nonzero; logs use stderr.

- On `server_not_installed` / `lsp_unavailable`, run `doctor <lang>` and read `serves`, not only `installed`. A configured absolute server command avoids a broken version-manager shim. After project config changes, `daemon restart --project` unloads only this project's sessions; its next request starts fresh servers. Active requests or pending server startup return `conflict`. Changes to the daemon's own configuration or inherited environment require a full `symora daemon restart`, which also unloads every other project's sessions; ask the user before running it.
- For persistent cross-package omissions, verify the project's environment and native server configuration. `doctor python` reports the supplied selection in `python_interpreter`; `serves` confirms only the handshake. Waiting does not prepare an environment, and a missing `.venv` alone does not prove a fault.
- On `unsupported`, choose an index, AST, or content-search alternative. On `conflict`, wait as directed and re-read. On `cancelled` with a content-change hint, resolve the target again. Repair `precondition_failed` and `config_errors` before depending on the result.
- `lsp.timeout_secs` is the shared LSP work budget: startup, requests, and retries consume it together. Narrow expensive queries or configure a larger budget; it does not lengthen the readiness wait, and repeated calls do not add a missing capability.

Output is one-line JSON when stdout is not a terminal; `--format pretty` indents it. `--check-version '<requirement>'` enforces a binary contract. Put global flags (`--format`, `--check-version`, `--deterministic`) after the subcommand (`symora refs src/auth.py:42 --format pretty`), where the allowed-tools patterns match.
