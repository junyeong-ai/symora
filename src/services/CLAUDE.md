# src/services — Backend Service Rules

These services back the CLI and MCP surfaces. They abstract over LSP transports, the SQLite index, and the structural search engine.

## Store durability

The SQLite index at `.symora/store.db` is product reliability, not a cache. Rules:

- `INIT_SCHEMA` is the authoritative shape. Bump `SCHEMA_VERSION` (and `PRAGMA user_version` in the SQL) whenever the on-disk shape changes.
- Schema mismatch triggers `recover_db` (rename to `.bak`, recreate). No hand-rolled `ALTER TABLE` migrations — they hide real failures behind expected duplicate-column errors.
- Never clear the index during normal daemon idle or shutdown. The index is what makes warm starts fast.
- `pack-cache.db` (in `services/pack_cache.rs`) is a separate, rebuildable cache. Open failures get logged at `warn!` and continue without the cache.

## LSP service abstraction

`LspService` is the trait every command speaks to. Two implementations:

- `DefaultLspService` — direct in-process LSP child processes.
- `DaemonLspService` — talks to a `symora daemon` over a Unix socket.

Both must produce identical results for the same inputs. Anything that diverges is a parity bug.

## Symbol cache invalidation

`SymbolCache` keys on `(path, content_hash)`. Editing a file changes the hash and invalidates the entry — don't add path-only invalidation paths that break this. Eviction is LRU with a configurable cap; size limits are enforced, not advisory.

## Fallback strategy

When an LSP server lacks a capability, return a structured "unsupported" response from the service layer. Don't fabricate data and don't fall through to text heuristics silently. Heuristic fallbacks (e.g. anchoring a `line:col` to the nearest symbol) are fine when they raise success rate without misleading.
