---
name: symora
description: Symbol-centric code navigation in this repository via the `symora` CLI — rough discovery, exact inspection, file overviews, references, context, usage, and impact analysis. JSON output.
when_to_use: User asks "where is this defined", "who calls this", "what would break if I change this", "show me this file's structure", or otherwise wants semantic answers instead of plain text search.
allowed-tools: Bash(symora *)
---

# Symora

Use `symora` when semantic code navigation is more useful than text search. Output is JSON — treat it as structured data.

## Two backends, different requirements

- **Index & structural search**: `search symbols`, `search content`, `search ast`, `map summary`, `map file`, `map dir`, `map related`. `search ast` and `map …` use tree-sitter and the file tree directly — no index, no language server. `search content` ranks the SQLite index and scans the filesystem when it isn't built. `search symbols` ranks the index and falls back to **LSP workspace symbols** when it isn't built — the one case in this group that needs the language server.
- **LSP-backed** (needs the language server installed for the target language): `symbols`, `def`, `refs`, `hover`, `callers`, `callees`, `typedef`, `implementations`, `rename`, `actions`, `signature`, `diagnostics`, `usage`, `context`, `impact`. Run `symora doctor <lang>` to confirm; install with the command in the doctor output.

Failures are structured: `{"error": {"code": "server_not_installed", "message": ..., "hint": ...}}` means the language server is missing — fall back to index-backed commands and follow the `hint`.

## Workflow

1. `symora search index status` — confirm the index is built. If `symbol_count: 0`, run `symora search index build` once.
2. `symora map summary` — project entrypoints and major areas.
3. `symora search symbols <query>` — rough workspace discovery (index-backed).
4. `symora map file <path>` — compact file overview. Outer fields (`siblings`, `related_files`, `counterpart_files`, `language`) are always valid; the embedded `symbols` field carries `{"error": {"code": "server_not_installed", ...}}` when the LSP is absent — parse the outer shape and ignore `symbols` in that case.
5. `symora symbols <file>` or `symora symbols --symbol <path>` — full semantic tree (LSP-backed).
6. `symora context | refs | usage` — exact follow-up from a location (LSP-backed).

## Command selection

### Rough discovery (file/symbol unknown)

```bash
symora search symbols AuthUser
symora search symbols 'SearchCommand/Content' --workspace-symbols
symora search content "async fn"
symora map summary
```

Narrow noisy results with `--kind`, `--lang`, or a more specific name.

### Exact inspection (file/symbol known)

```bash
symora symbols src/cli/commands/search/mod.rs --depth 2
symora symbols src/cli/commands/search/mod.rs --symbol 'SearchCommand/Content' --depth 2
symora hover src/cli/commands/search/mod.rs:30:10
symora def src/cli/commands/search/mod.rs:30:10
```

`symbols <file>` returns the full LSP tree. `symbols --symbol <path>` resolves an exact symbol path. Use `search symbols` for broad lookup, not `--name`.

### File and project overview

```bash
symora map summary
symora map file src/cli/commands/search/mod.rs --depth 1 --related-limit 5
symora map dir src/cli/commands
symora map related src/cli/commands/search/mod.rs --limit 5
```

`map file` is compact by design — use `symbols <file>` for the full tree. `map related` is a heuristic next-file hint, not a dependency graph.

### Exact follow-up from a location

```bash
symora context src/cli/commands/search/mod.rs:30 --all
symora refs src/cli/commands/search/mod.rs:30
symora usage src/cli/commands/search/mod.rs:30:10 --max-symbols 10 --limit 5
```

`context` reports unsupported features and points to a working alternative when the LSP lacks call hierarchy or type definition. `refs` accepts line-only inputs and resolves to the nearest symbol anchor. `usage` accepts either a `<pattern>` (regex/symbol name) or a `<file:line:col>` location, both LSP-backed, and auto-detects languages by file count when `--lang` is omitted. If no detected language has an installed server it returns a structured `server_not_installed` error — not a silent `count: 0`. When some languages were searched but others were missing, failed, or skipped once enough candidates were found, the result carries a `coverage_gaps` array of `{language, reason}` objects (`reason`: `server_not_installed | timed_out | unsupported | unavailable | not_searched`); a non-empty `coverage_gaps` means `count` is a lower bound — install the named server or narrow with `--lang`. An empty `usage` with neither an error nor `coverage_gaps` is a genuine zero.

### Refactor and health checks

```bash
symora actions list src/main.rs:42:5
symora actions apply src/main.rs:42:5 "Extract method"
symora rename src/main.rs:10:5 new_name --dry-run
symora edit replace-body src/main.rs:42:4 --body "$(cat new_fn.rs)" --dry-run
symora edit delete src/main.rs:42:4 --dry-run
symora diagnostics src/main.rs --with-context
symora impact src/main.rs:42
symora diff-impact
```

Mutating commands (`actions apply`, `rename`, and the `edit` subcommands) accept `--dry-run` for previews — the preview is an exact diff hunk. `edit delete` always reports references outside the deleted span that would dangle (`dangling_references` with the standard list shape; `references_status: "unsupported"|"unavailable"` when the check couldn't run). Add `--with-diagnostics` to any applied edit to attach post-edit LSP diagnostics: `{"status": "ok"|"unconfirmed"|"unsupported"|"unavailable", "count", "items"}` — an empty list under `unconfirmed` means *unknown*, not clean. The standalone `diagnostics` command carries the same `status` key only when the result is not authoritative.

## Output and global flags

List responses carry `count` (total found), `showing` (emitted), `items`, and—only when relevant—`truncated`, `stale`, `hints`, `next_commands` (ready-to-run follow-ups), and `indexing`. `indexing: "timed_out"` means the language server hadn't finished indexing: `count`/`items` are a lower bound, not complete — retry once the server is warm for the full set. `stale: true` (on `search symbols`/`search content`) means index-backed rows came from files that changed on disk since indexing — they may be out of date; re-run `symora search index build`. Global flags go **before** the subcommand:

```bash
symora --format compact search symbols AuthUser    # single-line JSON
symora -q rename src/main.rs:10:5 new_name --dry-run  # error-only
symora -v status                                   # verbose
```

Format values: `pretty` (default), `compact`. There is no `-c` shortcut.

## Index and daemon

```bash
symora search index status
symora search index build              # incremental: only changed files, prunes deleted
symora search index build --force --lang rust
symora daemon status
symora daemon restart
symora doctor                          # check environment; pass <language> to check one
```

Use these when search results are unexpectedly empty, a language server is unresponsive, or daemon/index state needs confirmation. Reserve `--force` for full rebuilds.

## When commands fail

- `count: 0` from `search …`: run `symora search index status` — if `symbol_count: 0` the index has never been built, run `symora search index build`.
- `server_not_installed` error: run `symora doctor <lang>` and install per its `install` field. While the LSP is missing, fall back to `search symbols`, `search content`, `map file`, `map dir`.
- `context` or `refs` reports an unsupported feature: follow the suggested fallback rather than retrying.
- `usage` errors with `server_not_installed`: no detected language had a server — install per `symora doctor <lang>`, or pass `--lang` to target an installed one.
- `usage` result carries `coverage_gaps`: coverage was partial, so `count` is a lower bound — install the named server, or ignore the gap if those languages are irrelevant.
- Search results truncated: narrow the query or raise `--limit`.

## Anti-patterns

- Using `-c` (does not exist) — use `--format compact` placed before the subcommand.
- `map file` when you actually need the full semantic tree → use `symbols <file>`.
- `symbols --name` for very broad discovery → use `search symbols`.
- Treating `map related` as a precise dependency graph.
- Retrying LSP-backed commands when the language server is missing → switch to index-backed commands.
- Assuming all language servers support call hierarchy and type definition equally — when a server lacks call hierarchy, `callers` falls back to reference-derived callers marked `callers_status: "references_derived"` (a broader approximation, not verified call edges).
