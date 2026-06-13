---
paths:
  - "src/cli/location.rs"
  - "src/cli/commands/**/*.rs"
  - "src/services/lsp/**/*.rs"
  - "src/services/ast_query.rs"
  - "src/services/daemon_lsp.rs"
  - "src/infra/lsp/**/*.rs"
  - "src/models/lsp.rs"
  - "src/models/symbol/**/*.rs"
---

# Position Indexing — Asymmetric and Encoding-Aware

CLI inputs and JSON outputs use **1-indexed lines** and **1-indexed Unicode-scalar columns** (one column per `char`). LSP wire values are **0-indexed**, and their columns are measured in the server's **negotiated `positionEncoding`** — UTF-8 bytes or UTF-16 code units. A column is therefore *transcoded*, not merely shifted by one. Every conversion is deliberate and lives at the LSP boundary.

## Two axes, converted differently

- **Line** — a flat shift. CLI → LSP `line - 1`; LSP → CLI `line + 1` (`saturating_sub(1)` guards the `u32` floor).
- **Column** — a scalar ↔ encoded-offset transcode against that line's text, keyed by the negotiated encoding. `column - 1` yields the *scalar* index only; turning a scalar index into a wire offset (or back) goes through `PositionConverter`, never raw arithmetic.

For pure ASCII the two collapse (scalar == byte == UTF-16 unit), which is why a missed transcode passes every ASCII test and corrupts the first line holding a multi-byte or non-BMP character.

## The encoding is negotiated, never assumed

`initialize` advertises `["utf-8", "utf-16"]`; the server's chosen encoding is read from its capabilities and held on the client (LSP 3.17 defaults to UTF-16 when the server is silent). Read it per request — don't hard-code UTF-16, and don't assume byte == char.

## Where conversions live

- `src/cli/location.rs` — parses `file:line:column` inputs (1-indexed scalar).
- `src/services/lsp/position.rs` — the boundary converter `PositionConverter`: `scalar_to_wire` (outbound scalar column → wire offset), `scalar_column` / `encoded_offset_to_scalar` (inbound wire offset → scalar column), `encoded_offset_to_byte` (wire offset → byte index for slicing), `floor_char_boundary` (clamp a stray offset to a char edge), seeded per file with `with_content`.
- `src/services/lsp/helpers.rs` — `to_lsp_position(line, column, content, encoding)` builds an outbound `Position`: `line - 1` plus `scalar_to_wire`.
- `src/services/lsp/converters.rs` — inbound LSP range → model `Location`: every returned offset is decoded through `PositionConverter` (`scalar_column`), then `+1` to scalar 1-indexed. Every new inbound reader follows this; an undecoded range is the bug below.
- `src/services/daemon_lsp.rs` ↔ `src/daemon/wire.rs` — the wire adds no conversion: each position crosses in whatever indexing its model type already carries (`Location` is 1-indexed scalar; raw LSP `Position`/`Range` payloads stay 0-indexed/encoded).

## The bug to avoid

A `±1` — or worse, an `offset + len` in wire units — buried in command logic away from the boundary. **Outbound:** build the `Position` from scalar columns via `to_lsp_position`; never advance a wire offset directly. **Inbound:** decode every returned range through `PositionConverter` before it reaches CLI/JSON; never emit a raw wire column. Verify against a file whose lines hold non-BMP characters (emoji, CJK) and exercise both ends of a range — a function on line 1 of ASCII proves nothing.
