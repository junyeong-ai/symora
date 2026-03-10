---
name: symora
version: 0.6.0
description: Use Symora for symbol-centric code navigation and analysis in this repository. Best for rough symbol discovery, exact symbol inspection, file overviews, references, context gathering, usage analysis, and impact analysis through the `symora` CLI.
allowed-tools: Bash(symora *)
---

# Symora

Use `symora` when semantic code navigation is more useful than plain text search.

Prefer this skill for:

- rough workspace symbol discovery
- exact symbol inspection from a file or symbol path
- compact file overviews before reading full source
- references, context, and usage analysis from a location
- impact analysis, code actions, and diagnostics

Symora is a CLI-first tool and returns JSON. Treat its output as structured data.

## Core workflow

Use commands in this order when possible:

1. `symora map summary` for project entrypoints and major areas
2. `symora search symbols <query>` for rough workspace discovery
3. `symora map file <path>` for a compact file overview
4. `symora symbols <file>` or `symora symbols --symbol <path>` for exact inspection
5. `symora context`, `symora refs`, or `symora usage` for exact follow-up

## Command selection

### Rough discovery

Use these when the file or exact symbol is not known yet.

```bash
symora search symbols AuthUser
symora search symbols 'SearchCommand/Content' --semantic
symora search content "async fn"
symora map summary
```

Notes:

- `search symbols` is the primary rough discovery command.
- Broad generic queries can be noisy. Prefer a more specific symbol name or add `--kind` / `--lang`.
- `search content` is useful when you know a phrase but not the symbol.

### Exact inspection

Use these when the file or symbol path is already known.

```bash
symora symbols src/cli/commands/search.rs --depth 2
symora symbols src/cli/commands/search.rs --symbol 'SearchCommand/Content' --depth 2
symora hover src/cli/commands/search.rs:30:10
symora def src/cli/commands/search.rs:30:10
```

Notes:

- `symbols <file>` is for semantic file inspection.
- `symbols --symbol <path>` is for exact symbol-path inspection.
- `symbols --name` is still available, but prefer `search symbols` for broad lookup.

### File and project overview

Use these before deeper inspection.

```bash
symora map summary
symora map file src/cli/commands/search.rs --depth 1 --related-limit 5
symora map dir src/cli/commands
symora map related src/cli/commands/search.rs --limit 5
```

Notes:

- `map file` is intentionally compact.
- Use `symbols <file>` if you need the full semantic tree.
- `map related` is heuristic. Treat it as a next-file hint, not a dependency graph.

### Exact follow-up from a location

These are the most useful commands once you know the target location.

```bash
symora context src/cli/commands/search.rs:30 --all
symora refs src/cli/commands/search.rs:30
symora usage src/cli/commands/search.rs:30:10 --max-symbols 10 --limit 5
```

Notes:

- `context` gathers nearby semantic information and provides fallback guidance when the active LSP server lacks call hierarchy or type-definition support.
- `refs` now resolves line-only inputs to the nearest symbol anchor when possible.
- `usage` accepts either a symbol query or a location and resolves the symbol automatically.

### Refactor and health checks

```bash
symora actions list src/main.rs:42:5
symora actions apply src/main.rs:42:5 "Extract method"
symora rename src/main.rs:10:5 new_name --dry-run
symora diagnostics src/main.rs --with-context
symora impact src/main.rs:42
symora diff-impact
```

## Practical guidance

- Prefer exact locations in `file:line:column` form for follow-up commands.
- If semantic file or location commands fail unexpectedly, check `symora doctor <lang>` before assuming the workflow is broken.
- If `context` or `refs` reports unsupported features, continue with the suggested fallback command instead of retrying the same command blindly.
- If `usage` resolves a location but returns no workspace results, continue with `symora symbols <file>` or `symora refs <location>`.
- If search results are truncated, narrow the query or increase `--limit`.
- If indexed search behaves unexpectedly, check or rebuild the index.

## Index and daemon operations

```bash
symora search index status
symora search index build
symora search index build --force --lang rust
symora daemon status
symora daemon restart
symora doctor
```

Use these when:

- search results are unexpectedly empty
- the language server becomes unresponsive
- you need to confirm daemon or index state
- repeated `search index build` is usually enough for refresh; reserve `--force` for a full rebuild

## Output expectations

Symora emits JSON and commonly uses fields such as:

- `count`
- `showing`
- `items`
- `truncated`
- `hints`

Use `-c` for compact JSON when token efficiency matters.

```bash
symora -c search symbols AuthUser
symora -c refs src/main.rs:10:5
```

## Anti-patterns

Avoid these mistakes:

- using `map file` when you actually need the full semantic tree (`symbols <file>`)
- using `symbols --name` for very broad discovery instead of `search symbols`
- treating `map related` as a precise dependency graph
- assuming all LSP servers support call hierarchy or type definition equally well
- retrying weak-LSP commands repeatedly instead of switching to file-level fallback flow
