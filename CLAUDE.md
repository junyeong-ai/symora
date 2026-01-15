# Symora - AI Agent Development Guide

LSP-based code intelligence CLI. Rust + async + daemon architecture.

## Architecture

```
src/
├── main.rs, app.rs       # Entry, DI container (App holds all services)
├── cli/commands/         # Command handlers (18 commands)
├── daemon/               # Unix socket server, JSON-RPC protocol
├── services/             # LspService trait, DaemonLspService, AstQueryService
├── infra/lsp/            # LSP client, 36 language server configs
├── models/               # Symbol, Location, Language, SymbolKind
└── error.rs              # LspError, SearchError
```

**Flow**: CLI → App → DaemonLspService → Unix Socket → DaemonServer → LspService → LSP Server

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

## AI Agent Commands

### Usage Finder
Search and analyze symbol usage with metrics:
```bash
symora usage "pattern" --lang rust           # Required: --lang
symora usage "*" --lang rust --filter no-docs  # Find undocumented symbols
symora usage "*" --lang rust --sort refs --with-metrics
```

Options:
- `--sort refs|name` - Sort by reference count or name
- `--filter has-tests,has-docs,no-docs,not-test-file` - Filter results
- `--with-metrics` - Include reference count, test/doc status
- `--max-symbols N` - Limit symbols to analyze (default: 50, for performance)
- `--limit N` - Limit output (default: 10)

### Enhanced Diagnostics
```bash
symora diagnostics src/main.rs --with-context     # AI-friendly error context
symora diagnostics src/main.rs --with-suggestions # Include fix suggestions
```

### Refactoring Actions
```bash
symora actions list src/main.rs:10:5              # All available actions
symora actions list src/main.rs:10:5 --kind refactor  # Refactoring only
symora actions apply src/main.rs:10:5 "Extract method"
```

### Pattern Edit (Structural)
Edit code using tree-sitter AST patterns:
```bash
symora edit pattern src/main.rs --pattern "function_item" --replacement "// DEPRECATED\n{match}"
```

- `{match}` placeholder for matched content
- Validates file size (100MB limit) and write permissions
- UTF-8 safe character handling

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
