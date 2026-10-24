# Symora — Agent Guide

Symora is a code-intelligence backend that an AI agent drives over two surfaces: a JSON-emitting CLI (`symora …`) and an MCP server (`symora mcp serve`). Both surfaces share one in-process command layer, so anything that holds for the CLI also holds for the MCP tool of the same shape.

This file contains repo-wide invariants. Module-specific rules live in nested `CLAUDE.md` files under `src/` and load on demand when you read files in those directories.

## Invariants

**1. Position indexing is asymmetric and encoding-aware.**
CLI inputs and JSON outputs use 1-indexed lines and 1-indexed Unicode-scalar columns. LSP wire values are 0-indexed, with columns in the server's negotiated `positionEncoding` (utf-8/utf-16) — a column is transcoded through the boundary converter, not merely shifted by one. A wrong direction, or a raw wire offset that escapes the converter, silently misplaces every reference, anchor, and edit. Detail: `.claude/rules/position-indexing.md`.

**2. JSON output is the public API.**
Field names, presence rules, and the shared list-response shape are stable contracts (the canonical field set lives in `.claude/rules/json-output-stability.md`). Treat renames the same way you'd treat a breaking signature change. Don't add decorative fields; an agent has to parse every key you emit.

**3. Daemon and direct execution must agree.**
The same command in the same repo must produce the same meaning whether it ran through `daemon serve` or in-process. Config loading, shared LSP deadlines, fallback paths, and error mapping agree across the mode boundary. Startup and retries consume the same budget; daemon RPCs forward the remaining allowance. Source application must finish or report partial application rather than being aborted midway by that LSP deadline.

**4. Fallback only when it raises success without misleading.**
A fallback that returns plausible but wrong data is worse than a clear "unsupported" message. When an LSP server lacks a capability, surface that in the response — don't synthesize one to hide it.

## Validation

For changes to Rust behavior, dependencies, or build logic:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

For documentation or comment-only changes, validate local links, rule path references, and changed examples or output claims against the matching binary. Check Rust formatting when Rust files are touched and skill frontmatter when a skill changes. Changes to bundled agent instructions also exercise the affected workflow. Rebuilds and full suites are needed when executable behavior changes, rather than for prose alone.

A change to the language-server layer or daemon also runs real-server suites: install rust-analyzer, then prepare the Python fixture and its pinned pyright:

```bash
(cd tests/fixtures/python/monorepo && npm ci --ignore-scripts && uv sync --frozen)
PATH="$PWD/tests/fixtures/python/monorepo/node_modules/.bin:$PATH" \
  cargo test --test lang_fixtures --test parity -- --include-ignored
```

A test that reaches a daemon runs its own copy of the binary: a daemon is per installation, so one a manual run of the same build left behind, started under another environment, would otherwise answer it.

For behavior changes, also exercise the affected commands against this repo (`cargo build` then `./target/debug/symora …`) and at least one external repo. Don't bake local-only repository assumptions into tests or docs.

Two ways a test here passes without testing anything, both worth checking for by injecting the defect and watching it fail. A test that skips itself when the environment cannot produce the condition — most of them, since permission bits do not constrain root — must decide that by probing the environment directly (`read_dir` on the blocked path, `pyright --version`), never by reading the output under test; a guard that consults the answer takes a defect for the environment and passes in silence. And an assertion that a field is ABSENT must first establish the command succeeded (`json_ok`), because an `{"error": ...}` response is missing every field and satisfies it just as well.

## Anti-goals

- Adding parallel command surfaces when an existing one can be extended.
- Tuning a heuristic from one attractive example without repeated evidence.
- Output changes that make agent parsing harder.
- Documenting derivable structure (file paths, command inventories, line counts).
- Backwards-compatibility shims. The on-disk schema bumps `PRAGMA user_version` and recovers; CLI flags evolve cleanly.
