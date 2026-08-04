# src/services — Backend Service Rules

These services back the CLI and MCP surfaces. They abstract over LSP transports, the SQLite index, and the structural search engine.

## Store durability

The SQLite index at `.symora/store.db` is product reliability, not a cache. Rules:

- `INIT_SCHEMA` is the authoritative shape. Bump `SCHEMA_VERSION` (and `PRAGMA user_version` in the SQL) whenever the on-disk shape changes.
- Schema mismatch triggers `recover_db` (rename to `.bak`, recreate). No hand-rolled `ALTER TABLE` migrations — they hide real failures behind expected duplicate-column errors.
- Never clear the index during normal daemon idle or shutdown. The index is what makes warm starts fast.
- Two rebuildable caches sit beside the store: `pack-cache.db` (`services/pack_cache.rs`) and `embeddings.db` (`services/embedding_cache.rs` — semantic-search vectors, bound to the active model id + dimension and reset on mismatch). Both rebuild from source, so a failure to *open* one degrades gracefully rather than failing the command: the embedding path logs at `warn!` and embeds in memory; the pack path treats it as a miss. Once open, the embedding cache surfaces operational errors (they propagate to the command), whereas the pack cache stays best-effort — a read/decode failure is a miss and a write/prune failure is logged at `debug!` and ignored.

The build scope recorded at index time is load-bearing, not bookkeeping: `indexed_languages` derives from it, and symbol search routes on that set — a covered language is answered from the index alone, an uncovered one is the only reason to pay for a live workspace query. Widening what a build claims to cover silently widens what search treats as authoritative.

## Test-versus-production classification

`services/test_scope.rs` answers two different questions and they must not be conflated. `is_test_file` is a path question, for ranking and whole-file summaries. `TestClassifier::is_test_code` is a position question, for anything that counts a reference as coverage rather than as a production dependency.

The position answer adds regions the LANGUAGE excludes from a production build (`infra/ast/test_regions.rs` — Rust's `#[cfg(test)]` and `#[test]`). That bound is deliberate: a compiler fact cannot produce a false positive, whereas a framework naming convention can, and code wrongly called test code deflates every coverage and risk signal downstream. Adding a language means finding its conditional-compilation rule, not its test-framework vocabulary.

## Import edges come from the parse tree

`services/imports.rs` reads a file's module references through a tree-sitter query per language, and `pack` builds its graph from those. A reference is only a reference where the grammar says one is: a Go constant holding a package path, an import written inside a comment, and a doc example are all text that scanning lines would have taken for structure, and PageRank amplifies whatever structure it is handed. Adding a language is one query plus the fixture that proves what it captures — never a new line rule.

## Service abstractions: LSP and Store

`LspService` and `StoreService` are the traits every command speaks to. Each has two interchangeable implementations, chosen once above the mode boundary:

- `Default*` — in-process (direct LSP child processes; the store's SQLite opens lazily on first use).
- `Daemon*` — forwards to a `symora daemon` over a Unix socket.

Both implementations of a trait must produce identical results for the same inputs — anything that diverges is a parity bug (invariant #3). A read against a missing store reports `NotInitialized` so the caller falls back to a filesystem scan; a present-but-unopenable store surfaces the real error.

## Symbol cache invalidation

`SymbolCache` keys on `(path, content_hash)`. Editing a file changes the hash and invalidates the entry — don't add path-only invalidation paths that break this. Eviction is oldest-first by insertion time (`created_at`, not touched on read) with a configurable `max_entries` cap; size limits are enforced, not advisory.

## Fallback strategy

When an LSP server lacks a capability, return a structured "unsupported" response from the service layer. Don't fabricate data and don't fall through to text heuristics silently. Heuristic fallbacks (e.g. anchoring a `line:col` to the nearest symbol) are fine when they raise success rate without misleading.
