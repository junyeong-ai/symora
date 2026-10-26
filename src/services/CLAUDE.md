# src/services — Backend Service Rules

These services back the CLI and MCP surfaces. They abstract over LSP transports, the SQLite index, and the structural search engine.

## Store durability

The SQLite index at `.symora/store.db` is product reliability, not a cache. Rules:

- `INIT_SCHEMA` is the authoritative shape. Bump `SCHEMA_VERSION` (and `PRAGMA user_version` in the SQL) whenever the on-disk shape changes.
- Schema mismatch triggers `recover_db` (rename to `.bak`, recreate). No hand-rolled `ALTER TABLE` migrations — they hide real failures behind expected duplicate-column errors.
- Never clear the index during normal daemon idle or shutdown. The index is what makes warm starts fast.
- A rebuildable cache sits beside the store: `pack-cache.db` (`services/pack_cache.rs`). It rebuilds from source, so it stays best-effort: a failure to open it, or a read/decode failure, is a miss, and a write/prune failure is logged at `debug!` and ignored.

Row mutation takes a turn rather than merging: `.symora/index.lock` (via `infra::file_lock`) is held exclusively by a build and by `clear`, shared by a per-file refresh, and waited on when the database file itself is replaced. The lock keeps the destructive windows from overlapping; the build epoch in `meta` keeps the completion marker honest when a lock did not hold — deleted, not carried by the filesystem — by guaranteeing the marker was written by whoever last owned the index. It does not fence row writes; that is the lock's job, and without one an overtaken build's rows outlive it until the next build prunes them. Neither replaces the other.

Whether the index may be answered from is the durable completion marker, read in the same transaction as the rows it qualifies — never a flag a handle caches, or the daemon and a direct run would disagree about a build the other made. Every operation that destroys rows opens a build window first (marker off, epoch advanced) and may publish completion only while it still owns that epoch, so an interrupted, cleared, or overtaken build leaves a store that reads as never built — slower, never wrong. A search answers for the requested languages the marker covers and says which those are, so whatever the caller reads live is disjoint from what the index served. That marker is also what lets a build without one behind it drop the text-index triggers and build the whole FTS index in one pass at the end (an order of magnitude cheaper than a row at a time): nothing consults an index that has not published, opening the store leaves incomplete builds untouched, and the next build takes the same path and rebuilds. Removing the completion marker and disabling triggers is one transaction; FTS rebuild, trigger restoration, and completion publication is another. Clearing an index disables triggers before removing content rows and resets FTS in the same transaction, including after an interrupted build. A build that *does* inherit a published index keeps them, because one pass over the whole table would cost more than the rows it changes.

The build scope recorded at index time is load-bearing, not bookkeeping: `content_languages` (the languages whose files the build indexed) and `indexed_languages` (those of them with a symbol extractor) derive from it, and search routes on those sets — a name in a covered language that the index matches is answered from the index alone; an uncovered language, or a name the index does not match, pays for a live workspace query or a scan of the tree. A path query (`/`, `[`) asks live servers whatever the coverage, and the index supplements its answer; `--workspace-symbols` asks live servers only. The unrestricted scope is every code language (`Language::is_code`), and a language's coverage is every extension it declares, never a hand-kept extension list that can drift from `Language::extensions`. Widening what a build claims to cover silently widens what search treats as authoritative.

Pack cache rows identify both extractors by release, grammar, and query. Validate the producer on each read so overlapping binary versions cannot replay incompatible rows. A missing cached signature list means extraction was deferred; an empty list means extraction completed and found none. Preserve complete cached declarations across per-request caps.

## Test-versus-production classification

`services/test_scope.rs` answers two different questions and they must not be conflated. `is_test_file` is a path question, for ranking and whole-file summaries. `TestClassifier::is_test_code` is a position question, for anything that counts a reference as coverage rather than as a production dependency.

The position answer adds regions the LANGUAGE excludes from a production build (`infra/ast/test_regions.rs` — Rust's `#[cfg(test)]` and `#[test]`). That bound is deliberate: syntax-based classification has a narrower scope than framework naming conventions; it does not establish executed coverage. Paths are classified relative to the project root so a checkout under an ancestor named `tests` does not classify production files as tests. Adding a language means finding its conditional-compilation rule, not its test-framework vocabulary.

## Declarations come from the parse tree

`services/symbols.rs` reads a file's declarations through one query per language, and the index, the AST fallback, signatures, pack and diff-impact all read what it reads, so a form it misses answers as absent wherever the index answers alone. The same rules hold in every language:

- A variable is a declaration where it names a module's or a member's value, never where a function's scope holds it; a local function or type is still read. Anchor the query where the language declares members rather than filtering its captures afterwards; a construct both a module and a function body can hold (Python's `if`, a C preprocessor conditional) is anchored, then judged by its nearest scope (`declares`).
- A name is what the grammar binds: an assignment declares only the names its targets bind, a destructuring pattern each name it binds, a C/C++ declarator its innermost name. The scan of a node's identifier children serves nodes named that way, never one whose own rule found no name.
- A symbol's container is the nearest enclosing declaration the query captures, a namespace passed through. A declaration's parameters and type are its header and contain nothing, except a type's own, which hold its members, and a record type it defines without a name of its own (`typedef struct { … } T`, Go's `inner struct{ … }`), which takes its name.
- A parameter declares a member only where its language says so: a modifier, `val`/`var`, a case class's first list, a promoted parameter, a record component.
- C and C++ are read again past a macro the grammar cannot expand, spelled as blanks so every offset holds, only where the grammar's structure proves it one (a `::` after a declarator's name, a prototype's parameters in a second pair of parentheses, a class key before a block read as a function), never by its spelling (capitals, an `_API` suffix); a preprocessor conditional after such a class's name is read the same way, through its first branch. The grammar's recovery is not local: a proved blank can still change the reading elsewhere, and what a file reads depends on the blanks taken together. Neither the bytes the re-read tree holds in errors nor the declarations it keeps tells a better reading from a worse one, so each blank needs a proof of its own rather than a check on the result.

`DECLARATION_FIXTURES` is the contract per language: a form or a language joins it with the query that reads it, and every pattern of a query matches a form its fixtures write. Where a language server is installed, its document symbols are the reference to compare a change against.

No two declarations of a file share a name position: the index keeps one row per position and the language-server join matches by it, so a declaration that writes no name of its own (an unnamed Dart primary constructor) is placed at syntax it does write.

`declaration_visibility` answers `is_exported`; its contract is in `.claude/rules/json-output-stability.md` (Symbols and analysis).

## Import edges come from the parse tree

`pack`'s import-resolution contract is in `.claude/rules/import-resolution.md`.

## Service abstractions: LSP and Store

`LspService` and `StoreService` are the traits every command speaks to. Each has two interchangeable implementations, chosen once above the mode boundary:

- `Default*` — in-process (direct LSP child processes; the store's SQLite opens lazily on first use).
- `Daemon*` — forwards to a `symora daemon` over a Unix socket.

Both implementations preserve the same result semantics and error contracts. Compare payloads under equivalent source and server state; indexing can change observations between calls (invariant #3). A read against a missing store reports `NotInitialized`, so the caller answers from its live source instead (a filesystem scan for content, language servers for symbols); a present-but-unopenable store surfaces the real error.

Rename captures its request-document hash before the retry loop and retains it across all attempts and server restarts.

## Symbol cache invalidation

`SymbolCache` holds one entry per path, valid only while the file's content hash matches the one it was computed from, so editing a file invalidates its entry — don't add path-only invalidation paths that break this. Entries also expire after a TTL, and eviction is oldest-first by insertion time (`created_at`, not touched on read) at the `max_entries` cap; size limits are enforced, not advisory.

## Fallback strategy

Position resolution follows the anchor contract in `src/cli/CLAUDE.md`; it must not select a nearby unrelated declaration.
