---
paths:
  - "src/cli/response/**/*.rs"
  - "src/cli/output*.rs"
  - "src/daemon/wire.rs"
  - "src/mcp/tools/**/*.rs"
---

# JSON Output Is the Public API

These files emit or shape the JSON that downstream agents parse. Treat any field rename, removal, or structure change as a breaking API change.

## Stable field names for list-style responses

- `count` — total matches found
- `showing` — number actually emitted in `items`
- `items` — the result array
- `truncated` — boolean indicating `showing < count`
- `hints` — optional next-step suggestions for the agent

If a new list response needs a different shape, that's a strong signal the underlying command should be reshaped instead.

## Field omission

Omit optional fields with `#[serde(skip_serializing_if = "Option::is_none")]`. Empty strings, zero values, or empty arrays for absent data force agents to write defensive parsing — don't make them.

## Wire vs response types

Two surfaces emit JSON:

- `src/cli/response/` — final user-facing output. Stability bar: API.
- `src/daemon/wire.rs` — daemon RPC envelope. Stability bar: API for the same reason.

These two layers must not drift from each other. If a CLI command grows a new output field, the wire type and the MCP handler that captures it stay aligned.
