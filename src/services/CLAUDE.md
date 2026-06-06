# src/services — Backend Service Rules

These services back the CLI and MCP surfaces. They abstract over LSP transports, the SQLite index, and the structural search engine.

## Store durability

The SQLite index at `.symora/store.db` is product reliability, not a cache. Rules:

- `INIT_SCHEMA` is the authoritative shape. Bump `SCHEMA_VERSION` (and `PRAGMA user_version` in the SQL) whenever the on-disk shape changes.
- Schema mismatch triggers `recover_db` (rename to `.bak`, recreate). No hand-rolled `ALTER TABLE` migrations — they hide real failures behind expected duplicate-column errors.
- Never clear the index during normal daemon idle or shutdown. The index is what makes warm starts fast.
- Two rebuildable caches sit beside the store: `pack-cache.db` (`services/pack_cache.rs`) and `embeddings.db` (`services/embedding_cache.rs` — semantic-search vectors, bound to the active model id + dimension and reset on mismatch). Both rebuild from source, so an open/read failure logs at `warn!` and continues without the cache — never block a command on one.

## Service abstractions: LSP and Store

`LspService` and `StoreService` are the traits every command speaks to. Each has two interchangeable implementations, chosen once above the mode boundary:

- `Default*` — in-process (direct LSP child processes; the store's SQLite opens lazily on first use).
- `Daemon*` — forwards to a `symora daemon` over a Unix socket.

Both implementations of a trait must produce identical results for the same inputs — anything that diverges is a parity bug (invariant #3). A read against a missing store reports `NotInitialized` so the caller falls back to a filesystem scan; a present-but-unopenable store surfaces the real error.

## Symbol cache invalidation

`SymbolCache` keys on `(path, content_hash)`. Editing a file changes the hash and invalidates the entry — don't add path-only invalidation paths that break this. Eviction is oldest-first by insertion time (`created_at`, not touched on read) with a configurable `max_entries` cap; size limits are enforced, not advisory.

## Fallback strategy

When an LSP server lacks a capability, return a structured "unsupported" response from the service layer. Don't fabricate data and don't fall through to text heuristics silently. Heuristic fallbacks (e.g. anchoring a `line:col` to the nearest symbol) are fine when they raise success rate without misleading.
