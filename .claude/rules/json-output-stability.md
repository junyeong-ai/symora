---
paths:
  - "src/cli/response/**/*.rs"
  - "src/cli/output*.rs"
  - "src/cli/commands/**/*.rs"
  - "src/daemon/wire*.rs"
  - "src/daemon/protocol.rs"
  - "src/mcp/tools/**/*.rs"
---

# JSON Output Is the Public API

These files emit or shape the JSON that downstream agents parse. Treat any field rename, removal, or structure change as a breaking API change.

## Stable field names for list-style responses

- `count` — total matches found
- `showing` — number actually emitted in `items`
- `items` — the result array
- `truncated` — boolean indicating `showing < count`
- `stale` — present (true) only when index-served rows came from files that changed on disk after indexing; re-run `symora search index build` to refresh
- `hints` — optional next-step suggestions for the agent
- `next_commands` — optional ready-to-run follow-up commands (omitted when empty)
- `indexing` — degradation marker, present only when the answer was computed under degraded workspace indexing (e.g. `"timed_out"`)
- `error` — structured `{code, message, hint}` failure, omitted on success

If a new list response needs a different shape, that's a strong signal the underlying command should be reshaped instead.

The boundary: an unbounded list — anything whose length scales with the codebase or the finding count (references, symbols, diagnostics) — wraps in `Section<T>`, usually flattened beside the response's own fields (`refs`, `usage`, `diagnostics` all do this). Bounded summary arrays (e.g. `impact`'s `files`, capped at `IMPACT_FILES_LIMIT` = 50) stay plain arrays.

## Field omission

Omit optional fields with `#[serde(skip_serializing_if = "Option::is_none")]`. Empty strings, zero values, or empty arrays for absent data force agents to write defensive parsing — don't make them.

## Doctor provenance fields

`doctor` rows carry `source: "config"` and `command` only when an `[lsp.servers.<lang>]` override applies — both omitted for builtin servers, and that absence is contract: it is how an agent detects an override that failed to apply. Both fields describe what the NEXT server start will use — a warm daemon keeps its startup table until `symora daemon restart`. Top-level `config_errors` (array of strings, omitted when empty) lists config problems affecting the report: rejected `[lsp.servers]` keys — recorded at load, never applied, never fatal to the rest of the config — and whole-config load failures (the report then reflects builtin defaults).

## Surfaces that emit JSON

- `src/cli/response/` — final user-facing output. Stability bar: API.
- `src/daemon/protocol.rs` — the JSON-RPC request/response envelope; `src/daemon/wire.rs` / `wire_error.rs` — the payload types it carries. Stability bar: API for the same reason.
- `src/mcp/tools/handlers.rs` — captures the shared command-layer JSON verbatim, so the MCP surface aligns structurally rather than by a parallel definition.

The first two must not drift from each other: if a CLI command grows a new output field, the wire type stays aligned. The MCP surface inherits the shape for free by capturing — don't reshape it there.
