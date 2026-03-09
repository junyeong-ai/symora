# Symora AI Coding Guide

Symora is a symbol-centric code intelligence CLI for AI coding agents. It combines LSP-based analysis, a Unix daemon for reusable language-server sessions, SQLite-backed search, and tree-sitter-based structural search.

This guide is for agents modifying Symora itself. Keep it factual, compact, and implementation-oriented.

## Product Model

- Symora is a CLI-first tool, not an MCP server.
- The primary user is an AI coding agent operating through shell commands.
- Outputs are machine-consumable JSON. Treat output shapes as an API.
- The project must work for both small repositories and large mixed-language repositories.

## Architecture

High-level flow:

- CLI parsing and command dispatch: `src/cli/`, `src/main.rs`
- App wiring and runtime services: `src/app.rs`
- LSP access:
  - direct mode: `src/services/lsp/`
  - daemon mode: `src/services/daemon_lsp.rs`, `src/daemon/`
- Search/indexing: `src/services/store/`
- AST search: `src/services/ast_query.rs`

Key directories:

- `src/cli/commands/` - command handlers
- `src/daemon/` - Unix socket RPC server/client and dispatch
- `src/services/lsp/` - LSP abstraction and direct implementation
- `src/services/store/` - SQLite-backed symbol/content index
- `src/models/` - shared domain types such as `Symbol`, `Language`, `Location`

## Core Design Rules

### 1. Keep CLI behavior deterministic

- The same command in the same repository should produce the same meaning whether it runs through the daemon or directly.
- Config loading, timeout calculation, and fallback behavior must stay aligned across execution modes.

### 2. Treat JSON output as a contract

- Do not casually rename fields or change response structure.
- List-like responses should keep stable semantics for fields such as `count`, `showing`, `truncated`, `hints`, and item arrays.
- Add guidance only when it reduces agent decision cost. Avoid decorative output.

### 3. Prefer exact semantic workflows over text heuristics

- Use location-first or symbol-path-first flows when possible.
- Broad discovery is useful, but exact follow-up should resolve to real symbols and positions.
- Keep heuristic ranking centralized and conservative.

### 4. Large-repo behavior matters

- Broad queries, language auto-detection, and concurrent LSP fan-out must remain practical on monorepos.
- Avoid changes that only look good on tiny repositories.

## Important Implementation Patterns

### Position indexing

- CLI locations are 1-indexed.
- LSP positions are 0-indexed.
- Be careful when translating line and column values.

### Symbol paths

- Symbol paths are an important user-facing addressing mechanism.
- `Symbol::compute_paths_for_all` and path-based matching are foundational for exact follow-up flows.
- Keep path semantics stable.

### Output helpers

- `OutputContext` handles project-relative paths and compact/quiet modes.
- Prefer returning structured values and letting output helpers serialize them.

### Fallback strategy

- Use fallback only when it increases success rate without producing misleading data.
- Good fallback examples in the current codebase:
  - location-to-symbol anchor resolution for `context`, `refs`, and `usage`
  - semantic or document-symbol fallback when indexed symbol search is insufficient
  - clearer unsupported-feature guidance in `context`

### Search and discovery heuristics

- Shared discovery logic lives in `src/cli/symbol_discovery.rs`.
- Keep broad-query handling, test/noise suppression, and common hint generation centralized there when possible.
- Do not scatter similar ranking logic across multiple commands unless there is a strong reason.

### Store durability

- Search index persistence is part of product reliability.
- Do not clear the index during normal daemon idle/shutdown behavior.
- Store open/reopen paths must preserve valid SQLite databases and avoid false corruption recovery.

## Current Stable User Flows

These flows are now important enough to preserve carefully:

- Broad symbol discovery: `search symbols`
- More specific workspace lookup: `symbols --name`
- Exact symbol inspection: `symbols --symbol`, `symbols <file>`
- File overview: `map file`
- Exact location follow-up: `context`, `refs`, `usage`
- Project overview: `map summary`

When changing these flows, prefer stability over novelty.

## Where to Be Careful

### Search / symbols / usage

- These commands now share discovery heuristics and guidance patterns.
- A change in one may unintentionally drift behavior from the others.

### Map commands

- `map file` is intended to be a compact overview, not a full symbol dump.
- If you need deep detail, that belongs in `symbols`.

### Context on weaker LSP servers

- Some languages or servers do not support call hierarchy or type definition well.
- Prefer better fallback messaging over pretending support exists.

## Config

Config priority:

- project config: `.symora/config.toml`
- global config: `~/.config/symora/config.toml`
- defaults

When changing config behavior:

- keep daemon and direct mode consistent
- avoid hidden mode-specific defaults

## Editing Guidance for Agents

- Prefer minimal, high-confidence changes.
- Do not add new commands unless a real repeated workflow gap exists.
- Prefer removing legacy overlap instead of keeping parallel command surfaces.
- Avoid tuning heuristics based on one attractive example. Use repeated evidence.
- Do not add maintenance-heavy facts to docs unless they materially help users or contributors.

## Recommended Validation Before Finishing

At minimum, verify changes with:

- `cargo fmt`
- `cargo test`
- `cargo build --quiet`

For behavior changes, also run a few real commands in:

- this repository (`./symora`)
- at least one large external repository during local validation

Do not encode local-only external repository assumptions into repository docs or tests.

## Anti-goals

- Do not keep adding broad heuristic tweaks without repeated evidence.
- Do not optimize docs for vanity metrics or volatile counts.
- Do not introduce output changes that make agent parsing harder.
- Do not add platform claims or feature guarantees that the code does not currently support.
