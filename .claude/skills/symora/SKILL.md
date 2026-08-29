---
name: symora
version: 0.21.0
description: Symbol-centric code navigation in this repository via the `symora` CLI — rough discovery, exact inspection, file overviews, references, context, usage, and impact analysis. JSON output.
when_to_use: User asks "where is this defined", "who calls this", "how does this function reach that one", "what would break if I change this", "show me this file's structure", or otherwise wants semantic answers instead of plain text search.
allowed-tools: Bash(symora *)
---

# Symora

Use `symora` when semantic code navigation is more useful than text search. Output is JSON by default — treat it as structured data.

## Two backends, different requirements

- **Index & structural search**: `search symbols`, `search content`, `search ast`, `map summary`, `map file`, `map dir`, `map related`. `search ast` uses tree-sitter directly, and the `map` family reads the file tree for structure (summary, siblings, directory layout) — no index, no language server. `search content` ranks the SQLite index for the languages it holds content for, and reads the working tree for the rest: for every code language the index does not cover (`backend: "scan"` items beside index ones), and for everything when the index isn't built or has no match — an index miss is confirmed against disk, so a zero is never a stale index's guess, and matches served as `scan` in a covered language mean the index is behind (rebuild it). Without `--lang` it searches code files; documentation and configuration formats (markdown, toml, yaml) are searched by naming one, and a zero says so. Three cases in this group reach the language server opportunistically (and degrade cleanly without it): `search symbols` is index-first but reaches **LSP workspace symbols** for any language the index does not cover, for every language when the index found nothing (a miss is not evidence of absence — a symbol written since the last build is in neither the index nor, unasked, the result), and whenever `--workspace-symbols` forces it; `map file`'s embedded `symbols` field is LSP-backed (its outer shape is not — see step 4); and related-file *ranking* (`map related`, and `map file`'s `related_files`) sharpens a filename/path heuristic with LSP symbol profiles — the file *set* needs no server, only its ordering.
- **LSP-backed** (needs the language server installed for the target language) — the main ones: `symbols`, `def`, `refs`, `hover`, `callers`, `callees`, `typedef`, `implementations`, `rename`, `actions`, `signature`, `diagnostics`, `usage`, `context`, `impact`. Run `symora doctor <lang>` (the same language ids and aliases `--lang` takes) to confirm — branch on `serves` (the server answered the LSP handshake), not on `installed` (a file resolves at that path, which a version-manager shim also satisfies); install with the command in the doctor output or point `[lsp.servers.<lang>]` at an existing binary. Each `doctor` row also carries `symbol_extraction` and `ast_search` booleans — whether the index and `search ast` cover that language with no server installed.

Failures are structured: `{"error": {"code": "server_not_installed", "message": ..., "hint": ...}}` means the language server is missing — fall back to index-backed commands and follow the `hint`.

## Workflow

1. `symora search index status` — confirm the index covers your languages. `languages` lists what a completed build covers and is what makes a symbol search's answer complete; if it is empty or missing yours, run `symora search index build` once. An `unread_paths` list names files or directories that build could not read, so the languages they could hold are covered only in part — fix their permissions and rebuild before trusting a zero.
2. `symora pack --tokens 4000` — token-budgeted repo brief (PageRank-ranked files with top-level signatures) — or `symora map summary` for a lighter entrypoint list. Either is the orientation step for a new task.
3. `symora search symbols <query>` — rough workspace discovery (index-backed).
4. `symora map file <path>` — compact file overview. Outer fields (`siblings`, `related_files`, `counterpart_files`, `language`) are always valid; the embedded `symbols` field carries `{"error": {"code": "server_not_installed", ...}}` when the LSP is absent — parse the outer shape and ignore `symbols` in that case.
5. `symora symbols <file>` — full semantic tree (LSP-backed) — or `symora symbols --symbol <path>`/`--name <name>` with no file: index-backed workspace resolution that finds methods too, with `--body`/`--signature`.
6. `symora context | refs | usage` — exact follow-up from a location (LSP-backed).

## Command selection

### Rough discovery (file/symbol unknown)

```bash
symora pack --tokens 4000                            # token-budgeted repo brief, strong first call on a new task
symora search symbols AuthUser
symora search symbols AuthUser --workspace-symbols   # force live LSP workspace symbols (skip the index)
symora search content "async fn"
symora map summary
```

Narrow noisy results with `--kind`, `--lang`, or a more specific name. `search symbols` matches flat names, `Class/method` paths, and `*` wildcards; `symbols --symbol`/`--name` resolves the same forms — workspace-wide from the index (methods included, with `--body`/`--signature`) when you give no file, or precisely within a file you name.

### Exact inspection (file/symbol known)

```bash
symora symbols src/cli/commands/search/mod.rs --depth 2
symora symbols src/cli/commands/search/mod.rs --symbol 'SearchCommand/Content' --depth 2
symora hover src/cli/commands/search/mod.rs:141:14
symora def src/cli/commands/search/mod.rs:141:14
```

`symbols <file>` returns the full LSP tree. `symbols --symbol <path>`/`--name <name>` resolves a specific symbol path (bare name, `Class/method` suffix, or `*` wildcard) — workspace-wide from the index when no file is given (methods included, `--body`/`--signature` populated), or within a named file. Use `search symbols` while the query is still broad; reach for `--symbol`/`--name` once the name is fairly specific.

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
symora context src/cli/commands/search/mod.rs:141 --all
symora context src/cli/commands/search/mod.rs:141 --callees --with-bodies   # callee list + complete bodies in one call
symora refs src/cli/commands/search/mod.rs:141
symora usage src/cli/commands/search/mod.rs:141:14 --max-symbols 10 --limit 5
symora callees src/cli/commands/search/mod.rs:141                  # direct callees (single hop)
symora callees src/cli/commands/search/mod.rs:141 --depth 3        # downward reachable set to depth 3
symora callees src/cli/commands/search/mod.rs:141 --to src/services/store/index.rs:42   # shortest call chain to a target
```

The location commands here and above — `refs`, `callers`, `callees`, `implementations`, `supertypes`, `subtypes`, `context`, `impact`, `usage` — accept a `file:line[:column]` and resolve it by one rule, the same one `edit` uses. A column-less `file:line` targets the symbol *declared* on that line (a method's declaration line means the method, never the impl/class spanning it), and a body line falls back to the enclosing symbol — so a `search symbols` result row can be passed straight through without pinning the exact name column. An explicit column is a precise address and reads exactly as `def`, `hover`, and `rename` read it: on a symbol's name it means that symbol; on any other token — a call site, a type in a signature, a receiver, an import, an attribute or decorator (a usage of the macro it names) — it means what that token denotes, resolved through its definition, so `refs`/`callers`/`impact` at a usage answer for the symbol used, anchored at its declaration, and `rename` at the same position renames the same thing; on no token at all but on a declaration's line (the keyword or whitespace before the name) it means that declaration. Where there is no token and no declaration — whitespace or a keyword inside a body, a comment — the column addresses nothing. When a line declares several symbols, these read commands analyze the first declaration and say so in `hints` (the resolved `target` is always echoed); only `edit` refuses with an ambiguity error, because a guessed write is destructive.

- `refs`, `context`, and `impact` echo what they resolved as a top-level `target`; when the position did not resolve to a listed symbol the placeholder carries `anchor_status` — `binding` (a local, parameter, or generated item the symbol tree does not list; the answer is exactly that binding's, anchored at its declaration), `not_a_symbol` (nothing there), or `unavailable` (a read failed) — omitted when it resolved, so a resolved query is never silently mistaken for a different symbol. `refs`, `callers`, `callees`, `implementations`, `supertypes`, and `subtypes` say the same in `hints` — including when a usage was resolved to its declaration — with the remedy for a position that denotes nothing (address the line's symbol with `file:line`). A usage of a symbol declared outside the project (a std or dependency item) anchors at that declaration — `target.file` is then absolute — while the references stay project-local. When a server's reference set omits the very usage you pointed at, the counts are a lower bound: `refs` sets a top-level `incomplete: true` and says so in `hints`, and `impact`/`context` set `refs.incomplete` (rust-analyzer does this for parameters of async functions).
- A position that denotes nothing — a column-less line that declares nothing and sits in no symbol, or a column on whitespace or a keyword — returns a placeholder `target` with `anchor_status: "not_a_symbol"`; follow the hint rather than retrying the same position.
- `context` reports unsupported features and points to a working alternative when the LSP lacks call hierarchy or type definition.
- `usage` accepts either a `<pattern>` (regex/symbol name) or a `<file:line[:col]>` location, both LSP-backed, and auto-detects languages by file count when `--lang` is omitted. If no detected language has an installed server it returns a structured `server_not_installed` error — not a silent `count: 0`.
- When some languages were searched but others were missing, failed, or skipped once enough candidates were found, the `usage` result carries a `coverage_gaps` array of `{language, reason}` objects (`reason`: `server_not_installed | timed_out | unsupported | unavailable | not_searched`); a non-empty `coverage_gaps` means `count` is a lower bound — install the named server or narrow with `--lang`. An empty `usage` with neither an error nor `coverage_gaps` is a genuine zero.
- `context --with-bodies` additionally attaches complete verbatim bodies: the target's body always attaches whole and unbudgeted; callee bodies (in listed order) and type bodies draw on the `--body-tokens` budget (default 2000), whole-body-or-nothing per item. `--with-bodies` is a `context` flag only — the standalone `callers`/`callees` commands do not accept it.
- Body-bearing sections report `bodies_included`; an item without `body` there was omitted for one of three causes: the token budget ran out, the symbol was unresolvable at its position, or it genuinely has no body (prototypes, interface methods). Only the first is cured by raising `--body-tokens` — an omission that persists after a large raise is not budget-caused; fetch a specific body with `symora symbols <file> --symbol 'Type/method' --body` (or with no file at all).
- `refs`, `context` (callers/callees sections), and `impact` emit gated `next_commands` — ready-to-run follow-ups — only when a condition holds (no usages found, truncation, single-file concentration, or an incomplete caller graph), so their presence is signal, never boilerplate.
- `callees` has three modes: direct (single hop, default), `--depth N` (the downward *reachable set* to depth N, carrying `max_depth_reached`/`callees_truncated` lower-bound markers), and `--to <file:line[:col]>` (the shortest call *chain* to a target). `--to` reports `reachability`: `found` (the ordered `chain` follows), `not_reached_within_bound` (raise `--depth`), or `no_static_path` (no chain through statically-resolved calls — still a lower bound; dynamic dispatch is not folded in).

### Refactor and health checks

```bash
symora actions list src/main.rs:42:5
symora actions apply src/main.rs:42:5 "Extract method"
symora rename src/main.rs:10:5 new_name --dry-run
symora edit replace-body src/main.rs --symbol 'load' --body "$(cat new_fn.rs)" --dry-run
symora edit replace-body src/main.rs:42:4 --body "$(cat new_fn.rs)" --dry-run
symora edit delete src/main.rs:42:4 --dry-run
symora edit delete src/main.rs:42:4 --expect-no-references
symora diagnostics src/main.rs --with-context --with-suggestions
symora impact src/main.rs:42
symora diff-impact
```

Mutating commands (`actions apply`, `rename`, and the `edit` subcommands) accept `--dry-run`.

- `edit replace-body` (and MCP `replace_symbol_body`) replaces the symbol's ENTIRE definition span — signature, braces, and body. Pass the complete definition as `--body`, not just the inner code.
- Each `edit` subcommand names its source argument for what that argument is: `replace-body --body <complete definition>`, `insert-before`/`insert-after` `--code <lines>`, `replace --text <raw range replacement>`, `pattern --text <replacement for each AST match>` (with `--pattern` and `--lang`). The MCP tools carry the same names as properties (`body`, `code`).
- The `edit` subcommands target by `<file> --symbol <path>` or by `file:line[:col]`. `<path>` matches like every `--symbol` surface — bare name, `Class/method` suffix, `*/method` wildcard, or the exact `name_path` returned by `search`/`symbols` (a method on a type reads `Type/method` consistently, including Rust `impl` blocks). Prefer `--symbol` over `file:line`: it re-resolves against the live file, so sequential edits in one file don't invalidate each other — line addressing goes stale after every edit.
- A `file:line` target without a column addresses the symbol declared on that line (a method's declaration line edits the method, not its impl/class); a body line falls back to the enclosing symbol, and the attribute or doc-comment lines a declaration's range opens with belong to it; several declarations on one line are an ambiguity error — pass the column. A `file:line:col` target must be on the declaration itself (its keyword through its name); a column inside a body or on the indent is a structured `not_found`, never a guess at the enclosing block.
- For `edit`, the preview is an exact diff hunk; `rename` and `actions apply` instead report the files they would touch (`affected_files`/`files_changed` and a `changes` list), not a hunk.
- `edit replace` additionally accepts `--expect <text>`: the splice is refused unless the live text at the range matches exactly (only `\r\n` vs `\n` is tolerated).
- A `conflict` error from any edit or rename means the file changed against the revision that was analyzed (a stale range, or an `--expect` mismatch) — the one edit failure that re-reading and retrying fixes.
- `edit delete` always reports references outside the deleted span that would dangle (`dangling_references` with the standard list shape; `references_status: "unsupported"|"unavailable"` when the check couldn't run).
- With `--expect-no-references`, verified reference-freedom becomes a precondition: the delete is refused (no write, `precondition_failed` error) when dangling references exist, when the check is `unsupported`/`unavailable`, or when an indexing-degraded zero leaves it unverified — the message says which, and the hint carries the next command. Unlike `conflict`, re-reading and retrying alone will not clear it.
- Add `--with-diagnostics` to any applied edit to attach post-edit LSP diagnostics: `{"status": "ok"|"unconfirmed"|"unsupported"|"unavailable", "count", "items"}` — an empty list under `unconfirmed` means *unknown*, not clean. The standalone `diagnostics` command carries the same `status` key only when the result is not authoritative.
- Add `--verify-callers` to an applied (non-dry-run) `edit replace-body`/`insert-before`/`insert-after` to pull post-edit diagnostics on the edited symbol's callers — a read→edit→verify loop that catches a signature break at its call sites (caller files are capped and the cap disclosed). Ignored on `--dry-run`.
- `diagnostics --with-context` attaches each finding's surrounding code; `--with-suggestions` attaches the LSP's fix suggestions. Both are opt-in.
- `impact` on a trait/interface method reports `blast_radius.dynamic_dispatch` (`status: "incomplete"` with the implementation count, or `"unavailable"`) — caller counts are then a lower bound and `confidence` is capped.

## Output and global flags

List responses carry `count` (total found), `showing` (emitted), `items`, and—only when relevant—`truncated`, `stale`, `incomplete`, `hints`, `next_commands` (ready-to-run follow-ups), and `indexing`. `incomplete: true` means `count` itself is a lower bound — the answer does not hold everything its own sources held — and the leading `hints` name the cause and, where one exists, `next_commands` carries the fix. Causes: paths the walk could not read (named in the hint), an index built while some paths could not be read, a cap that stopped the search before it ran out (`--limit`'s widening, an index page, `usage`'s `--max-symbols`), or two overlapping sources at least one of which was not read whole. It is a different question from `stale`/`indexing`: those say a source is behind, this says the answer is short. `map summary`, `map file`, `map dir`, and `pack` carry the same flag at the top level with an `unread_paths` list beside it instead of hints — check those paths' permissions, since no rebuild reaches a path nothing can read. `indexing: "timed_out"` means the language server hadn't finished indexing: `count`/`items` are a lower bound, not complete — retry once the server is warm for the full set. `stale: true` (on `search symbols`/`search content`) means index-backed rows came from files that changed on disk since indexing — they may be out of date; re-run `symora search index build`. Responses are size-capped at `output.max_response_chars` (default 20000 chars of emitted JSON, 0 disables; set under `[output]` in `.symora/config.toml`): when the cap fires, whole items are dropped from the largest list, `truncated` is set, `count` stays the true total, and a hint names the key — `--format compact` fits more items under the same cap. The ceiling only ever drops whole list items — a response without a list section is never sliced or truncated mid-field. Global flags may be placed before the subcommand:

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
                                       # `unread_paths` in the result: the files/directories that could not be read, so the index does not cover them
symora search index build --force --lang rust
# One build owns the index at a time, across processes: a second one (or a
# `clear` beside it) gets `conflict` — retry, nothing was half-applied.
symora daemon status
symora daemon restart
symora doctor                          # check environment; pass <language> to check one
```

Use these when search results are unexpectedly empty, a language server is unresponsive, or daemon/index state needs confirmation. Reserve `--force` for full rebuilds.

## When commands fail

- `count: 0` from `search …`: run `symora search index status` — if `languages` is empty, no build has completed (row counts alone can't tell you: a per-file refresh leaves rows behind without covering anything), so run `symora search index build`. If `languages` omits the language you're searching, the answer came from a live language server, not the index.
- `search symbols` returns `count: 0` with a hint saying the index is being rebuilt: the index took no part in the answer because a build owns it. Wait for `symora search index status` to report `is_indexing: false` and re-run — building again is not the remedy, since one is already running.
- `search symbols` returns `count: 0` with a hint naming a language: the zero is not exhaustive for that language — either it has no index extractor and no live language server, or the search ran workspace-only (index not built, or `--workspace-symbols`) and the live lookup failed, or enough matches came from other languages that this one was never searched. `next_commands` carries the remedy that fits the cause: `search content`/`doctor`, `search index build` when building the index can actually cover the language, re-running without `--workspace-symbols`, or re-running with `--lang <language>` for one that was never searched. These hints are coverage-driven, and so is their absence: a bare `count: 0` means every requested language was answered — either from the index in this call or by its language server — so the zero really is "nothing exists". A language the answer could not verify appears in `coverage_gaps` with the reason, and in a hint, whether the result is empty or partial: an index that returned no match vouches for nothing, however wide its build scope, which is why a dead server turns even a covered language's zero into a disclosed lower bound.
- `server_not_installed` error: run `symora doctor <lang>` and install per its `install` field. A row with `installed: true` and `serves: false` means the binary resolves but cannot run — usually a version-manager shim; run it directly to see why, then point `[lsp.servers.<lang>]` at the real binary. If the binary exists but is off PATH (nvm/mise/asdf, hermetic CI), set `command = "/absolute/path"` under `[lsp.servers.<lang>]` in `.symora/config.toml` (key = the `language` id doctor prints), run `symora daemon restart`, then confirm with `symora doctor <lang>` — the row shows `source: "config"`; if it doesn't, check the top-level `config_errors` for the rejected key or field. A config override has NO observable effect — including any error you are testing for — until the daemon restarts; the warm daemon keeps its startup server table. While the LSP is missing, fall back to `search symbols`, `search content`, `map file`, `map dir`.
- `context` or `refs` reports an unsupported feature: follow the suggested fallback rather than retrying.
- `incomplete: true` on any list result: the `count` is a lower bound, not the whole set. Read the leading hint for which of the causes it is. When it names paths, fix their permissions and re-run — `search index build --force` is offered only when the paths are readable now, so its absence beside a hole means a rebuild cannot help yet.
- `conflict` error from `edit`/`rename`: the file changed since it was analyzed — re-read it (`symora symbols <file>` or `map file`) and retry with fresh coordinates. Recoverable; do not treat it as a hard failure.
- `precondition_failed` error from `edit delete --expect-no-references`: the symbol is not verified reference-free. If the message counts references, fix or remove those call sites (the hint's `symora refs …` lists them) and retry; if it says the check was unsupported/unavailable/degraded, verify manually and rerun without the flag. Unlike `conflict`, do not blind-retry.
- `usage` errors with `server_not_installed`: no detected language had a server — install per `symora doctor <lang>`, or pass `--lang` to target an installed one.
- `usage` result carries `coverage_gaps`: coverage was partial, so `count` is a lower bound — install the named server, or ignore the gap if those languages are irrelevant.
- Search results truncated: narrow the query or raise `--limit`.
- `truncated` with a hint naming `output.max_response_chars`: the response hit the size ceiling — narrow the query, switch to `--format compact`, follow `next_commands` when present, or raise the ceiling under `[output]` in `.symora/config.toml`.

## Anti-patterns

- Using `-c` (does not exist) — use `--format compact` placed before the subcommand.
- Addressing an `edit` by line when you already hold a symbol path → pass `--symbol 'Class/method'`; line numbers go stale after every edit.
- `map file` when you actually need the full semantic tree → use `symbols <file>`.
- `symbols --name` for very broad discovery → use `search symbols`.
- Treating `map related` as a precise dependency graph.
- Retrying LSP-backed commands when the language server is missing → switch to index-backed commands.
- Assuming all language servers support call hierarchy and type definition equally — when a server lacks call hierarchy, `callers` falls back to reference-derived callers marked `callers_status: "references_derived"` (a broader approximation, not verified call edges).
