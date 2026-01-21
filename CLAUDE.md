# Symora - AI Agent Development Guide

LSP-based code intelligence CLI. Rust + async + daemon architecture.

## Architecture

```
src/
├── main.rs, app.rs       # Entry, DI container (App holds all services)
├── cli/commands/         # Command handlers (20+ commands)
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

### Add Search Operation
1. `services/store/schema.rs` — Add SQL query constant
2. `services/store/index.rs` — Add public method
3. `daemon/handlers.rs` — Add params struct
4. `daemon/protocol.rs` — Add method constant
5. `daemon/server.rs` — Add handler
6. `daemon/client.rs` — Add client method
7. `cli/commands/search.rs` — Add CLI subcommand

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

### Centralized Timeout Calculation
```rust
// daemon/client.rs - Language-aware timeout with operation multipliers
fn calculate_timeout(file: Option<&Path>, method: &str) -> Duration {
    let language = file.map(Language::from_path).unwrap_or(Language::Unknown);
    let lsp_method = methods::to_lsp_method(method).unwrap_or("textDocument/hover");
    config::timeout_for(language, lsp_method)
}

// daemon/protocol.rs - Map daemon method to LSP method
pub fn to_lsp_method(daemon_method: &str) -> Option<&'static str> {
    match daemon_method {
        FIND_REFS => Some("textDocument/references"),
        CALLS_INCOMING => Some("callHierarchy/incomingCalls"),
        // ...
    }
}
```

### Generic Section Pattern
```rust
// context.rs - Consistent section structure for optional data
#[derive(Debug, Serialize)]
pub struct ContextSection<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T> ContextSection<T> {
    fn success(items: Vec<T>) -> Self { Self { items, error: None } }
    fn error(msg: impl Into<String>) -> Self { Self { items: vec![], error: Some(msg.into()) } }
}
```

### Config-Based Limits
```rust
// context.rs - Derive limits from config with From trait
struct ContextLimits { calls: usize, refs: usize, tests: usize }

impl From<&LspConfig> for ContextLimits {
    fn from(cfg: &LspConfig) -> Self {
        Self { calls: cfg.calls_limit, refs: cfg.refs_limit, tests: cfg.tests_limit }
    }
}
```

### Insert Mode for Edit Operations
```rust
// edit.rs - Mode-aware positioning for insert operations
#[derive(Clone, Copy)]
enum InsertMode {
    After,   // Insert at end position (end_line, end_column)
    Before,  // Insert at start position (line, column)
}

// Helper functions for target resolution
fn resolve_file_path(app: &App, target: &str) -> PathBuf
async fn lookup_symbol_by_path(app: &App, file: &Path, pattern: &str) -> Result<Symbol>
async fn find_symbol_at_position(app: &App, file: &Path, line: u32) -> Result<Symbol>
```

### Refs Fallback for Call Hierarchy
```rust
// calls.rs - Automatic fallback when call hierarchy unsupported
async fn execute_incoming(location, limit, no_fallback, app) {
    match app.lsp.incoming_calls(...).await {
        Ok(calls) => { /* use call hierarchy */ },
        Err(e) if !no_fallback && is_unsupported(&e) => {
            // Fallback to references + filter callable symbols
            incoming_calls_from_refs(app, file, line, column, limit).await
        },
        Err(e) => { /* report error */ }
    }
}
```

## Config

| Type | Path |
|------|------|
| Project | `.symora/config.toml` |
| Global | `~/.config/symora/config.toml` |

Priority: Project > Global > Defaults

### Key Config Values
```toml
[lsp]
timeout_secs = 60       # Base timeout (default: 60s, increased for monorepos)
refs_limit = 500        # Max references per query
calls_limit = 100       # Max call hierarchy items
tests_limit = 10        # Max test results in context

[test]
file_patterns = ["_check.rs", "Verify.java"]  # Custom test file suffixes
dir_patterns = ["/verification/"]              # Custom test directories
markers = ["@MyTest"]                          # Custom test markers
```

Built-in test patterns support 25+ languages: JUnit, Kotest, xUnit, pytest, RSpec, Jest, etc.

## LSP Support Matrix

| Feature | Rust | Go | Java | TS/JS | Kotlin | Python | PHP | C/C++ |
|---------|:----:|:--:|:----:|:-----:|:------:|:------:|:---:|:-----:|
| symbol/def | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ |
| refs | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ✅ | ✅ |
| hover | ✅ | ✅ | ✅ | ⚠️ | ✅ | ⚠️ | ✅ | ✅ |
| calls | ✅ | ✅ | ✅ | ⚠️ | ❌ | ❌ | ❌ | ⚠️ |
| rename | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ❌ | ❌ | ✅ |

**Notes**:
- Kotlin: Class-level symbols only; use AST search for methods
- Python: Large monorepo may timeout; use AST search as fallback
- TypeScript: Initial requests slow (15s+); subsequent requests fast
- PHP: Rename requires Intelephense Premium
- Call hierarchy fallback: Uses refs when LSP doesn't support callHierarchy

## AST Search (tree-sitter)

13 languages: Python, TypeScript/TSX, JavaScript, Rust, Go, Java, Kotlin, C++, C#, Bash, Ruby, Lua, PHP

## Store Module (SQLite)

LIKE-based search with SQLite. Index stored at `.symora/search.db`.

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
| `calls incoming` | Refs fallback | Auto-fallback when call hierarchy unsupported |

### Large Codebases
- Use `--max-symbols 30` for faster usage analysis
- TypeScript: First request ~15s (compilation), subsequent <1s
- Python: Large monorepos may timeout; use AST search fallback
- Default timeout increased to 60s for better monorepo support
