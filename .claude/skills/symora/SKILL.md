---
name: symora
metadata:
  version: 0.29.0
description: Use the symora CLI for symbol discovery, exact code navigation, references, caller graphs, context, and symbol-aware edits.
allowed-tools: Bash(symora *)
---

# Symora

Symora emits structured JSON. Use its index and AST for discovery and the target language server for semantic follow-up. It answers within the selected project, language, ignore policy, and size limits; a zero outside that scope is not established.

## Choose the next query

```bash
symora search index status
symora map summary
symora pack --tokens 4000
symora search symbols AuthService --limit 10
symora symbols src/auth.py --signature
symora symbols src/auth.py --symbol 'AuthService/login' --body
symora refs src/auth.py:42
symora context src/auth.py:42 --all
symora impact src/auth.py:42 --depth 1
```

Build the index with `symora search index build` when status lists no completed language coverage, when the answer names an out-of-date index, or after external edits requiring a current workspace-wide search. A build is incremental; `--force` is for rebuilding everything. If another build owns the index, wait for it rather than starting another.

- `search symbols` discovers names and paths. `symbols <file> --symbol 'Type/method'` inspects a known declaration. Workspace resolution without a file reads the index and may consult a language server.
- `search content` searches code by default. Name a documentation or data language explicitly: `symora search content 'config key' --lang markdown`. Hidden paths are excluded from discovery, including `.github` and `.claude`; use a direct file read or an appropriate text search for those.
- `search ast` uses tree-sitter queries and needs no language server. `symora search nodes --list` lists supported grammars.
- `symbols` and `map file` prefer document symbols and fall back to AST declarations; `backend` names the producer. AST output is flat, with containment in `name_path`.
- `map related` ranks possible next files; it is not a dependency graph. `pack` ranks files using imports and emits AST-derived signatures within an approximate character-based token budget, not a model-specific tokenizer count.
- `--deterministic` restricts answers to the index and built-in grammars. Semantic commands without such a source return `unsupported`.

## Resolve locations precisely

Inputs and outputs use 1-indexed lines and Unicode-scalar columns.

For `refs`, `callers`, `callees`, `implementations`, type hierarchy, `context`, `impact`, and location-based `usage`:

- `file:line` addresses the declaration on that line, or the enclosing symbol on a body line.
- `file:line:column` addresses the token there. A usage resolves through its definition. Keywords or whitespace in a declaration header may address that declaration; an empty body position addresses nothing.
- Read the echoed `target`, `anchor_status`, and hints before using an answer. A binding is a local or parameter that the document-symbol tree does not list.

`def`, `hover`, `signature`, `typedef`, and `rename` are position-exact; an omitted column means column 1. Take a name position from an actual result rather than guessing.

```bash
symora callees src/auth.py:42 --depth 3
symora callees src/auth.py:42 --to src/store.py:18
symora usage src/auth.py:42 --max-symbols 10 --limit 5
symora diff-impact
symora diff-impact main
symora diff-impact --staged
```

`diff-impact [REV]` compares tracked working-tree changes with one revision, not a revision range. Running from a subdirectory scopes the analysis there. Check `unmeasured_files` and truncation before interpreting its totals.

## Read limits before making a decision

Lists use `count`, `showing`, and `items`. Optional markers change how to interpret them:

- `truncated`: some found items were not emitted. Narrow the query, raise `--limit`, or follow the size-limit hint.
- `incomplete`: the count is a lower bound within its sources; read the cause in hints or `unread_paths`.
- `coverage_gaps`: named languages were not fully searched.
- `stale`: emitted index rows came from changed files. A separate index-currency hint can also report newly added files that have no rows yet. Rebuild before treating index results as a current enumeration.
- `indexing`: `timed_out` means work exceeded the wait; `unconfirmed` means the server did not confirm workspace readiness. Retry may help, but servers without a completion channel can remain unconfirmed. Neither state proves absence. Rename preserves unconfirmed on server-provided edits; timed_out blocks application.
- `degraded_column`: the reported column could not be decoded from readable source; re-read before using it as an edit address.

`impact` reports references and a bounded static caller graph. `max_depth_reached`, `callers_truncated`, `incomplete`, and `dynamic_dispatch` disclose limits. It does not estimate the probability a change is safe. Test references and test callers are not execution coverage. String-based registration, reflection, structural Protocol implementations, external consumers, and serialized contracts may require additional inspection.

When call hierarchy is unsupported, `callers` may return `callers_status: references_derived`: containing functions of references, not verified call edges. `--no-fallback` requires actual call hierarchy.

`context --with-bodies` attaches complete bodies. The target is unbudgeted; callee/type bodies share `--body-tokens` (default 2000). Missing bodies may also be unresolvable or absent in source, so raising the budget is not always a remedy.

## Edit and verify

```bash
symora edit replace-body src/auth.py --symbol 'AuthService/login' --body - --dry-run < replacement.py
symora edit delete src/auth.py --symbol 'obsolete' --expect-no-references --dry-run
symora rename src/auth.py:42:9 new_name --dry-run
symora diagnostics src/auth.py --with-context --with-suggestions
```

- `replace-body --body` replaces the entire definition, including its signature, decorators/attributes, and body. Read it with `symbols --body`, modify that complete definition, and pass it back.
- Prefer `--symbol 'Type/method'` over line addresses for sequential edits. An edit column must address a declaration, not a token inside its body. Ambiguous targets are refused.
- `insert-before`/`insert-after` take `--code`; raw range `replace` and AST `pattern` take `--text`. `replace --expect` requires the live range to match.
- Symbol/range edit previews contain exact hunks. Rename and code-action previews report affected files and edit counts.
- `delete --expect-no-references` refuses when reference freedom is not verified. A zero static reference count alone does not prove runtime unreachability.
- Applied edits can attach `--with-diagnostics`; symbol replacement/insertion can also use `--verify-callers`. `unconfirmed`, `unsupported`, or `unavailable` diagnostics are not a clean result. Project type checking and tests remain necessary for behavior changes.
- Edits in one project are serialized. Files are checked for observed changes before replacement. An unversioned edit to another project file not open in the language server has no verifiable server-time source baseline. Multi-file application is not an atomic filesystem transaction; on failure inspect the named files already modified before retrying.

## Recover from errors

Failures emit `{ "error": { "code", "message", "hint" } }` on stdout with a nonzero exit. Stderr carries logs. Follow the specific hint:

- `server_not_installed` / `lsp_unavailable`: run `symora doctor <lang>`. Branch on `serves`, not `installed`; version-manager shims can resolve without running. Configure an absolute `[lsp.servers.<lang>].command` when necessary, then restart a warm daemon.
- `unsupported`: use the available index/AST alternative; retrying the same missing capability does not add support.
- `conflict`: another edit/build owns the resource or a file changed. Wait as directed, re-read, and retry with current input.
- `precondition_failed`: fix the named precondition; do not blind-retry.
- `config_errors`: repair the configuration before depending on its intended behavior.

Use `--format compact` for one-line JSON; there is no `-c` flag. `--check-version '<requirement>'` enforces a binary version requirement. `symora setup skill` installs the playbook bundled with that binary; `--git-ref` explicitly selects a different source.
