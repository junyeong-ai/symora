---
paths:
  - "src/cli/location.rs"
  - "src/cli/commands/**/*.rs"
  - "src/services/lsp/**/*.rs"
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
- `src/services/lsp/converters.rs` — the canonical CLI↔LSP boundary: `+1` on the way out of LSP, `saturating_sub(1)` on the way in.
- `src/services/daemon_lsp.rs` ↔ `src/daemon/wire.rs` — the daemon wire carries 1-indexed display values; the client side converts back to 0-indexed model ranges.

Keep conversions on these boundaries. A `+1`/`-1` on a line or column anywhere else is a layering bug. (Converting a 1-indexed line to a 0-based array index inside a command is not a boundary conversion and is fine.)
