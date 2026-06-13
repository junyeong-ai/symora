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
- `truncated` — boolean indicating `showing < count`, whether from an item cap (`--limit`, config limits) or from the per-response size ceiling (`output.max_response_chars`, applied once in the output layer as the last step before emission).
  - A size-driven reduction always appends a hint naming the config key.
  - Items are only ever dropped whole — per-item shape never changes with response size.
  - Layering: command-level content budgets are token-denominated and apply first inside the command (e.g. `pack --tokens`); the char-denominated transport ceiling applies last and wins. Don't add a third budget vocabulary.
- `stale` — present (true) only when index-served rows came from files that changed on disk after indexing; re-run `symora search index build` to refresh
- `hints` — optional next-step suggestions for the agent
- `next_commands` — optional ready-to-run follow-up commands (omitted when empty); also emitted as a top-level field by non-list outputs (`map` summary, `impact`)
- `bodies_included` — present only on sections where body attachment ran (`context --with-bodies`) and that still contain items: always equals the number of `items` carrying a complete `body`.
  - Items without `body` under it were omitted — token budget exhausted, symbol unresolvable at the item's position, or genuinely bodiless (prototypes, interface methods) — disclosed, never silent.
  - If the transport size ceiling drops items from the section, the fitter recounts this field against the remaining items (removing it when the section empties), so the equality holds in every emitted response.
- `indexing` — degradation marker, present only when the answer was computed under degraded workspace indexing (e.g. `"timed_out"`)
- `error` — structured `{code, message, hint}` failure, omitted on success

If a new list response needs a different shape, that's a strong signal the underlying command should be reshaped instead.

The boundary: an unbounded list — anything whose length scales with the codebase or the finding count (references, symbols, diagnostics) — wraps in `Section<T>`, usually flattened beside the response's own fields (`refs`, `usage`, `diagnostics` all do this). Bounded summary arrays (e.g. `impact`'s `files`, capped at `IMPACT_FILES_LIMIT`) stay plain arrays.

## Field omission

Omit optional fields with `#[serde(skip_serializing_if = "Option::is_none")]`. Empty strings, zero values, or empty arrays for absent data force agents to write defensive parsing — don't make them.

## Doctor provenance fields

`doctor` rows carry `source: "config"` and `command` only when an `[lsp.servers.<lang>]` override applies — both omitted for builtin servers, and that absence is contract: it is how an agent detects an override that failed to apply. Both fields describe what the NEXT server start will use — a warm daemon keeps its startup table until `symora daemon restart`. Top-level `config_errors` (array of strings, omitted when empty) lists config problems affecting the report: rejected `[lsp.servers]` stanzas (non-canonical keys, unknown fields, mistyped values; a stanza's unknown fields and mistyped values are reported together in one load, each message a single line) — recorded at load, never applied, never fatal to the rest of the config — and whole-config load failures (the report then reflects builtin defaults). `config show` emits the same `config_errors` array under the same name and presence rule for the rejected-stanza class, so an agent inspecting config sees the rejection where it looks first. `symbol_extraction` / `ast_search` — always-present booleans on each `languages[]` row: whether the compiled-in index extractor and the tree-sitter AST grammar cover the language (static build facts, independent of server install state).

## Surfaces that emit JSON

- `src/cli/response/` — final user-facing output. Stability bar: API.
- `src/daemon/protocol.rs` — the JSON-RPC request/response envelope; `src/daemon/wire.rs` / `wire_error.rs` — the payload types it carries. Stability bar: API for the same reason.
- `src/mcp/tools/handlers.rs` — captures the shared command-layer JSON verbatim, so the MCP surface aligns structurally rather than by a parallel definition.

The first two must not drift from each other: if a CLI command grows a new output field, the wire type stays aligned. The MCP surface inherits the shape for free by capturing — don't reshape it there.
