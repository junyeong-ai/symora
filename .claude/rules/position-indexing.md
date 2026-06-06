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

# Position Indexing — Asymmetric on Purpose

CLI inputs and JSON outputs use **1-indexed** lines and columns. LSP wire values use **0-indexed**. Every conversion site must be deliberate.

## Direction matters

- CLI → LSP: `line - 1`, `column - 1` (with `saturating_sub(1)` so 0 doesn't underflow `u32`).
- LSP → CLI: `line + 1`, `column + 1`.

## End-of-line conventions

LSP `Range::end` is exclusive. When converting a range to CLI form, the off-by-one for the end position depends on whether the consumer wants inclusive or exclusive — pick one per command and stay consistent.

## Why this gets wrong silently

A single missed conversion shifts every reference, anchor, and edit by one line or column. Tests that operate on small files (a function on line 1) often won't catch it. Verify against a multi-line file with content that exercises both ends.

## Where conversions live

- `src/cli/location.rs` — parses `file:line:column` input strings (1-indexed).
- `src/services/lsp/converters.rs` — LSP → display: `+1` as model `Location`s are built from LSP ranges. The display → LSP direction is `to_lsp_position` in `src/services/lsp/helpers.rs` (`saturating_sub(1)`), applied where a CLI position is sent into an LSP request.
- `src/services/daemon_lsp.rs` ↔ `src/daemon/wire.rs` — the wire adds no conversions: `wire.rs` copies each position through as-is, carrying whatever indexing its model type uses (`Location` is 1-indexed; raw LSP `Position`/`Range` payloads stay 0-indexed). The one client-side `saturating_sub(1)` is `daemon_lsp.rs::diagnostics`, which carries 1-indexed values on the wire and rebuilds a 0-indexed `Range`.

Keep conversions at these boundaries — the LSP request/response edge, or a command's own output edge when it emits LSP-native ranges (e.g. `folding`, `inlay-hints`, `format`). A `+1`/`-1` buried in computation logic, away from a boundary, is the layering bug to avoid. (Converting a 1-indexed line to a 0-based array index inside a command is not a boundary conversion and is fine.)
