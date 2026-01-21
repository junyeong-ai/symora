# Symora - AI Agent Development Guide

LSP-based code intelligence CLI. Rust + async + daemon architecture.

## Architecture

```
src/
├── main.rs, app.rs       # Entry, DI container (App holds all services)
├── cli/commands/         # Command handlers (20 commands)
├── daemon/               # Unix socket server, JSON-RPC protocol
├── services/             # LspService trait, DaemonLspService, AstQueryService
│   └── store/            # SQLite Store (symbols, content search)
├── infra/lsp/            # LSP client, 36 language server configs
├── models/               # Symbol, Location, Language, SymbolKind
└── error.rs              # LspError, SearchError, StoreError
```

**Flow**: CLI → App → DaemonLspService → Unix Socket → DaemonServer → LspService → LSP Server
**Search Flow**: CLI → DaemonClient → DaemonServer → Store → SQLite

## Extension Points

### Add Command
1. `cli/commands/{name}.rs` — Args struct + `execute(args, app)` async fn
2. `cli/commands/mod.rs` — `pub mod {name}`
3. `cli/mod.rs` — Add to `Commands` enum
4. `main.rs` — Add match arm

### Add Language
1. `models/symbol.rs` — Add to `Language` enum, `from_extension()`, `lsp_id()`
2. `infra/lsp/servers.rs` — Add `ServerConfig` in `defaults()`

### Add LSP Operation
1. `services/lsp.rs` — Add to `LspService` trait + implement
2. `services/daemon_lsp.rs` — Add RPC wrapper method
3. `daemon/protocol.rs` — Add method constant
4. `daemon/server.rs` — Add dispatch handler

## Critical Patterns

### Position Indexing
CLI uses 1-indexed, LSP uses 0-indexed:
```rust
Position::new(line.saturating_sub(1), col.saturating_sub(1))
```

### Output
```rust
ctx.print_success_flat(response)  // JSON to stdout
ctx.print_error(msg)              // JSON error
ctx.relative_path(path)           // Strip project root from paths
```

### Symbol Path (Serena-compatible)
```rust
Symbol::compute_paths_for_all(&mut symbols);
Symbol::filter_by_path(&symbols, "*/update");  // Wildcard match
Symbol::find_by_path(&symbols, "Foo/bar");     // Exact match
```

### Error Recovery
```rust
// Automatic retry on server termination
self.manager.execute_with_retry(language, |client| async move {
    client.request(...).await
}).await
```

`LspError::is_recoverable()` → retry possible
`LspError::needs_restart()` → requires server restart

### File I/O
```rust
// Single-pass validation + read (size check, binary detection)
read_file_validated(file).await?

// Edit file validation (size + write permission check)
validate_file_for_edit(path)?  // MAX_EDIT_FILE_SIZE = 100MB
```

### UTF-8 Safe String Handling
```rust
// Convert character index to byte index for safe slicing
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(s.len())
}
```

### Concurrent LSP Requests
```rust
// Semaphore-based concurrency control for parallel LSP calls
let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_LSP_REQUESTS));
let futures: Vec<_> = symbols.iter().map(|s| {
    let sem = Arc::clone(&semaphore);
    async move {
        let _permit = sem.acquire().await.ok()?;
        // LSP call here
    }
}).collect();
join_all(futures).await
```

## Config

| Type | Path |
|------|------|
| Project | `.symora/config.toml` |
| Global | `~/.config/symora/config.toml` |

Priority: Project > Global > Defaults

### Test File Detection Config
Custom patterns for test file detection (used by `usage`, `impact`, `context`, `diff-impact`):
```toml
[test]
file_patterns = ["_check.rs", "Verify.java"]  # Custom file suffixes
dir_patterns = ["/verification/"]              # Custom directory patterns
markers = ["@MyTest"]                          # Custom test markers
```

Built-in patterns support 25+ languages: JUnit, Kotest, xUnit, pytest, RSpec, Jest, etc.

## LSP Support Matrix

| Feature | Rust | Go | Java | TS/JS | Kotlin | Python | PHP | C/C++ |
|---------|:----:|:--:|:----:|:-----:|:------:|:------:|:---:|:-----:|
| symbol/def | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ |
| refs | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ✅ | ✅ |
| hover | ✅ | ✅ | ✅ | ⚠️ | ✅ | ⚠️ | ✅ | ✅ |
| calls | ✅ | ✅ | ✅ | ⚠️ | ❌ | ❌ | ❌ | ⚠️ |
| rename | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ❌ | ❌ | ✅ |

**Notes**:
- Kotlin: Class-level symbols only; use `symora search ast` for methods
- Python: Large monorepo may timeout; use AST search as fallback
- TypeScript: Initial requests slow (15s+); subsequent requests fast
- PHP: Rename requires Intelephense Premium

## AST Search (tree-sitter)

13 languages: Python, TypeScript/TSX, JavaScript, Rust, Go, Java, Kotlin, C++, C#, Bash, Ruby, Lua, PHP

```bash
symora search ast "function_item" --lang rust
symora search nodes --lang csharp  # list node types
```

## BM25 Search (SQLite FTS5)

Ranked search using SQLite FTS5 with BM25 scoring. Index stored at `.symora/search.db`.

```bash
# Symbol search (searches symbol names with BM25 ranking)
symora search symbols "execute"                  # basic search
symora search symbols "Handler" --kind class     # filter by kind
symora search symbols "process" --limit 10       # limit results

# Content search (searches code lines with BM25 ranking)
symora search content "async fn"                 # basic search
symora search content "TODO" --lang rust         # filter by language
symora search content "error" --limit 20         # limit results

# Index management
symora search index build                        # build/update index
symora search index build --force                # force full rebuild
symora search index build --lang rust,python     # specific languages
symora search index status                       # show index stats
symora search index clear                        # clear index
```

### Store Module Structure
```
src/services/store/
├── mod.rs           # Public exports (Store, types)
├── db.rs            # Async SQLite wrapper (tokio-rusqlite)
├── schema.rs        # DDL, search queries (WAL mode)
├── index.rs         # Store implementation
├── symbols.rs       # SymbolExtractor (tree-sitter)
└── types.rs         # StoreConfig, SymbolSearchResult, ContentSearchResult
```

### Add Search Operation
1. `services/store/schema.rs` — Add SQL query constant
2. `services/store/index.rs` — Add public method
3. `daemon/handlers.rs` — Add params struct
4. `daemon/protocol.rs` — Add method constant
5. `daemon/server.rs` — Add handler
6. `daemon/client.rs` — Add client method
7. `cli/commands/search.rs` — Add CLI subcommand

## Impact Analysis

### Context Command
Gather all related context for a symbol in a single call:
```bash
symora context src/main.rs:16:5 --all      # Include callers, callees, types, tests
symora context src/main.rs:16:5 --callers  # Include only callers
symora context src/main.rs:16:5 --callees  # Include only callees
```

### Diff-Impact Command
Analyze impact of git diff changes. Parses git diff to identify changed functions/methods, then uses LSP to find all references and callers:
```bash
symora diff-impact                          # Compare against HEAD
symora diff-impact main                     # Compare against main branch
symora diff-impact --staged                 # Analyze staged changes only
symora diff-impact --callers                # Include caller analysis
symora diff-impact --max-symbols 30         # Limit symbols analyzed
```

Output includes:
- Changed files and symbols count
- Test coverage ratio (symbols with/without test references)
- Per-symbol: reference count, test vs production references, callers

## Key Types

- `SymbolKind`: function, class, method, field, struct, enum, interface, module, property, constructor, variable, constant, enum_member, type_parameter
- `Language`: 36 languages with aliases (e.g., `typescript`/`ts`, `python`/`py`)
- Location format: `file:line:column` (all 1-indexed in CLI)

## Performance Notes

| Command | Optimization | Notes |
|---------|--------------|-------|
| `usage` | Semaphore parallelization | 20 concurrent LSP requests |
| `usage` | `--max-symbols N` | Limit analysis scope (default: 50) |
| `edit pattern` | UTF-8 char indexing | Safe multi-byte character handling |
| `diagnostics` | Lazy context loading | `--with-context` fetches on demand |

### Large Codebases
- Use `--max-symbols 30` for faster usage analysis
- TypeScript: First request ~15s (compilation), subsequent <1s
- Python: Large monorepos may timeout; use AST search fallback
