---
paths:
  - "src/cli/location.rs"
  - "src/cli/analysis.rs"
  - "src/cli/utils/symbol_nav.rs"
  - "src/cli/response/mod.rs"
  - "src/cli/commands/**/*.rs"
  - "src/services/lsp/**/*.rs"
  - "src/services/ast_query.rs"
  - "src/services/symbols.rs"
  - "src/services/store/**/*.rs"
  - "src/services/daemon_lsp.rs"
  - "src/daemon/wire.rs"
  - "src/infra/lsp/**/*.rs"
  - "src/models/lsp.rs"
  - "src/models/symbol/**/*.rs"
---

# Position Indexing — Asymmetric and Encoding-Aware

CLI inputs and JSON outputs use **1-indexed lines** and **1-indexed Unicode-scalar columns** (one column per `char`). LSP wire values are **0-indexed**, and their columns are measured in the server's **negotiated `positionEncoding`** — UTF-8 bytes or UTF-16 code units. A column is therefore *transcoded*, not merely shifted by one. Every conversion is deliberate: a wire column is transcoded at the LSP boundary, and a tree-sitter byte column where a grammar reader builds a position (`scalar_position` in `services/symbols.rs`, `char_column` in `services/ast_query.rs`).

## Two axes, converted differently

- **Line** — a flat shift. CLI → LSP `line - 1`; LSP → CLI `line + 1` (`saturating_sub(1)` guards the `u32` floor).
- **Column** — a scalar ↔ encoded-offset transcode against that line's text, keyed by the negotiated encoding. `column - 1` yields the *scalar* index only; turning a scalar index into a wire offset (or back) goes through the transcoders in `services/lsp/position.rs`, never raw arithmetic.

For pure ASCII the two collapse (scalar == byte == UTF-16 unit), which is why a missed transcode passes every ASCII test and corrupts the first line holding a multi-byte or non-BMP character.

## The encoding is negotiated, never assumed

`initialize` advertises `["utf-8", "utf-16"]`; the server's chosen encoding is read from its capabilities and held on the client (LSP 3.17 defaults to UTF-16 when the server is silent). Read it per request — don't hard-code UTF-16, and don't assume byte == char.

## Where conversions live

- `src/cli/location.rs` parses `file:line[:column]` (1-indexed scalar; an omitted column is tracked by `column_explicit`).
- `src/services/lsp/position.rs` holds the only scalar ↔ wire transcoders. Outbound positions are built with `to_lsp_position` (`src/services/lsp/helpers.rs`). Every inbound range is decoded through `PositionConverter::scalar_column_disclosed` (`src/services/lsp/converters.rs`), which yields the scalar column and the `degraded` flag `Location::degraded_column` carries; a new inbound reader that builds a `Location` does the same. A raw model `Position`, which carries no flag, decodes through `scalar_offset`.
- The daemon wire (`src/services/daemon_lsp.rs` ↔ `src/daemon/wire.rs`) adds no conversion: `Location` crosses 1-indexed scalar, native edit ranges 0-indexed scalar. Do not transcode them again because their type is `Position` or `Range`.

## Degradation is disclosed, never silently guessed

The inbound read path degrades to the raw wire offset when a cross-file result's line is unreadable (the request's own file is always seeded, so this is rare). That guess is **disclosed**: `PositionConverter::scalar_column_disclosed` returns `(column, degraded)`, and every converter-built location threads the flag onto an optional `degraded_column: bool` (omitted when false) that survives to the JSON and across the daemon wire (`models::Location` ↔ `daemon::wire::Location` ↔ `LocationOutput::from_location`). So an agent trusts a column absolutely unless `degraded_column` says it is a guess — the read-path analogue of the edit path's fail-closed `scalar_offset_checked`. A location built from synthesized coordinates (no source `Location`) uses `LocationOutput::from_path` and is never degraded.

## The bug to avoid

A `±1` — or worse, an `offset + len` in wire units — buried in command logic away from the boundary. **Outbound:** build the `Position` from scalar columns via `to_lsp_position`; never advance a wire offset directly. **Inbound:** decode every returned range through `PositionConverter` before it reaches CLI/JSON; never emit a raw wire column. Verify against a file whose lines hold non-BMP characters (emoji, CJK) and exercise both ends of a range — a function on line 1 of ASCII proves nothing.
