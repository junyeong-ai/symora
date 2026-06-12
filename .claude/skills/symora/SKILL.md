---
name: symora
version: 0.11.0
description: Symbol-centric code navigation in this repository via the `symora` CLI — rough discovery, exact inspection, file overviews, references, context, usage, and impact analysis. JSON output.
when_to_use: User asks "where is this defined", "who calls this", "what would break if I change this", "show me this file's structure", or otherwise wants semantic answers instead of plain text search.
allowed-tools: Bash(symora *)
---

# Symora

Use `symora` when semantic code navigation is more useful than text search. Output is JSON — treat it as structured data.

## Two backends, different requirements

- **Index & structural search**: `search symbols`, `search content`, `search ast`, `map summary`, `map file`, `map dir`, `map related`. `search ast` uses tree-sitter directly, and the `map` family reads the file tree for structure (summary, siblings, related files, directory layout) — no index, no language server. `search content` ranks the SQLite index and scans the filesystem when it isn't built. Two cases in this group still reach the language server: `search symbols` falls back to **LSP workspace symbols** when the index isn't built, and `map file`'s embedded `symbols` field is LSP-backed (its outer shape is not — see step 4).
- **LSP-backed** (needs the language server installed for the target language): `symbols`, `def`, `refs`, `hover`, `callers`, `callees`, `typedef`, `implementations`, `rename`, `actions`, `signature`, `diagnostics`, `usage`, `context`, `impact`. Run `symora doctor <lang>` to confirm; install with the command in the doctor output or point `[lsp.servers.<lang>]` at an existing binary. Each `doctor` row also carries `symbol_extraction` and `ast_search` booleans — whether the index and `search ast` cover that language with no server installed.

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
symora search symbols AuthUser --workspace-symbols   # force live LSP workspace symbols (skip the index)
symora search content "async fn"
symora map summary
```

Narrow noisy results with `--kind`, `--lang`, or a more specific name. `search symbols` matches flat names; a `Class/method` path resolves through `symbols --symbol` (below), not `search`.

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

The location commands here and above — `refs`, `callers`, `callees`, `context`, `impact`, `usage` — accept a `file:line` (column optional) and resolve it to the enclosing symbol's name, so a `search symbols` result row can be passed straight through without pinning the exact name column. `refs`, `context`, and `impact` echo what they resolved as a top-level `target` (with `resolved: false` on a placeholder), so a snapped query is never silently mistaken for a different symbol. (`def`, `hover`, `typedef` stay position-exact, since you may target a specific token mid-line.) `context` reports unsupported features and points to a working alternative when the LSP lacks call hierarchy or type definition. `usage` accepts either a `<pattern>` (regex/symbol name) or a `<file:line:col>` location, both LSP-backed, and auto-detects languages by file count when `--lang` is omitted. If no detected language has an installed server it returns a structured `server_not_installed` error — not a silent `count: 0`. When some languages were searched but others were missing, failed, or skipped once enough candidates were found, the result carries a `coverage_gaps` array of `{language, reason}` objects (`reason`: `server_not_installed | timed_out | unsupported | unavailable | not_searched`); a non-empty `coverage_gaps` means `count` is a lower bound — install the named server or narrow with `--lang`. An empty `usage` with neither an error nor `coverage_gaps` is a genuine zero. `context --with-bodies` additionally attaches complete verbatim bodies — target, then callees in listed order, then the type — whole-body-or-nothing under `--body-tokens` (default 2000). Body-bearing sections report `bodies_included`; an item without `body` there was omitted for one of three causes: the token budget ran out, the symbol was unresolvable at its position, or it genuinely has no body (prototypes, interface methods). Only the first is cured by raising `--body-tokens` — an omission that persists after a large raise is not budget-caused; fetch a specific body with `symora symbols <file> --body`. `refs`, `context` (callers/callees sections), and `impact` emit gated `next_commands` — ready-to-run follow-ups — only when a condition holds (declaration-only result, truncation, single-file concentration, or an incomplete caller graph), so their presence is signal, never boilerplate.

### Refactor and health checks

```bash
symora actions list src/main.rs:42:5
symora actions apply src/main.rs:42:5 "Extract method"
symora rename src/main.rs:10:5 new_name --dry-run
symora edit replace-body src/main.rs:42:4 --body "$(cat new_fn.rs)" --dry-run
symora edit delete src/main.rs:42:4 --dry-run
symora edit delete src/main.rs:42:4 --expect-no-references
symora diagnostics src/main.rs --with-context
symora impact src/main.rs:42
symora diff-impact
```

Mutating commands (`actions apply`, `rename`, and the `edit` subcommands) accept `--dry-run`. For `edit`, the preview is an exact diff hunk; `rename` and `actions apply` instead report the files they would touch (`affected_files`/`files_changed` and a `changes` list), not a hunk. `edit replace` additionally accepts `--expect <text>`: the splice is refused unless the live text at the range matches exactly (only `\r\n` vs `\n` is tolerated). A `conflict` error from any edit or rename means the file changed against the revision that was analyzed (a stale range, or an `--expect` mismatch) — the one edit failure that re-reading and retrying fixes. `edit delete` always reports references outside the deleted span that would dangle (`dangling_references` with the standard list shape; `references_status: "unsupported"|"unavailable"` when the check couldn't run). With `--expect-no-references`, verified reference-freedom becomes a precondition: the delete is refused (no write, `precondition_failed` error) when dangling references exist, when the check is `unsupported`/`unavailable`, or when an indexing-degraded zero leaves it unverified — the message says which, and the hint carries the next command. Unlike `conflict`, re-reading and retrying alone will not clear it. Add `--with-diagnostics` to any applied edit to attach post-edit LSP diagnostics: `{"status": "ok"|"unconfirmed"|"unsupported"|"unavailable", "count", "items"}` — an empty list under `unconfirmed` means *unknown*, not clean. The standalone `diagnostics` command carries the same `status` key only when the result is not authoritative. `impact` on a trait/interface method reports `blast_radius.dynamic_dispatch` (`status: "incomplete"` with the implementation count, or `"unavailable"`) — caller counts are then a lower bound and `confidence` is capped.

## Output and global flags

List responses carry `count` (total found), `showing` (emitted), `items`, and—only when relevant—`truncated`, `stale`, `hints`, `next_commands` (ready-to-run follow-ups), and `indexing`. `indexing: "timed_out"` means the language server hadn't finished indexing: `count`/`items` are a lower bound, not complete — retry once the server is warm for the full set. `stale: true` (on `search symbols`/`search content`) means index-backed rows came from files that changed on disk since indexing — they may be out of date; re-run `symora search index build`. Responses are size-capped at `output.max_response_chars` (default 20000 chars of emitted JSON, 0 disables; set under `[output]` in `.symora/config.toml`): when the cap fires, whole items are dropped from the largest list, `truncated` is set, `count` stays the true total, and a hint names the key — `--format compact` fits more items under the same cap. Global flags go **before** the subcommand:

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
- `search symbols` returns `count: 0` with a hint naming a language: that language has no index extractor and no live language server — the zero is not exhaustive for it; run the `search content`/`doctor` commands from `next_commands`.
- `server_not_installed` error: run `symora doctor <lang>` and install per its `install` field. If the binary exists but is off PATH (nvm/mise/asdf, hermetic CI), set `command = "/absolute/path"` under `[lsp.servers.<lang>]` in `.symora/config.toml` (key = the `language` id doctor prints), run `symora daemon restart`, then confirm with `symora doctor <lang>` — the row shows `source: "config"`; if it doesn't, check the top-level `config_errors` for a rejected key. While the LSP is missing, fall back to `search symbols`, `search content`, `map file`, `map dir`.
- `context` or `refs` reports an unsupported feature: follow the suggested fallback rather than retrying.
- `conflict` error from `edit`/`rename`: the file changed since it was analyzed — re-read it (`symora symbols <file>` or `map file`) and retry with fresh coordinates. Recoverable; do not treat it as a hard failure.
- `precondition_failed` error from `edit delete --expect-no-references`: the symbol is not verified reference-free. If the message counts references, fix or remove those call sites (the hint's `symora refs …` lists them) and retry; if it says the check was unsupported/unavailable/degraded, verify manually and rerun without the flag. Unlike `conflict`, do not blind-retry.
- `usage` errors with `server_not_installed`: no detected language had a server — install per `symora doctor <lang>`, or pass `--lang` to target an installed one.
- `usage` result carries `coverage_gaps`: coverage was partial, so `count` is a lower bound — install the named server, or ignore the gap if those languages are irrelevant.
- Search results truncated: narrow the query or raise `--limit`.
- `truncated` with a hint naming `output.max_response_chars`: the response hit the size ceiling — narrow the query, switch to `--format compact`, follow `next_commands` when present, or raise the ceiling under `[output]` in `.symora/config.toml`.

## Anti-patterns

- Using `-c` (does not exist) — use `--format compact` placed before the subcommand.
- `map file` when you actually need the full semantic tree → use `symbols <file>`.
- `symbols --name` for very broad discovery → use `search symbols`.
- Treating `map related` as a precise dependency graph.
- Retrying LSP-backed commands when the language server is missing → switch to index-backed commands.
- Assuming all language servers support call hierarchy and type definition equally — when a server lacks call hierarchy, `callers` falls back to reference-derived callers marked `callers_status: "references_derived"` (a broader approximation, not verified call edges).
