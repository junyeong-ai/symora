# Symora - AI Agent Development Guide

LSP-based code intelligence CLI. Rust + async + daemon architecture.

## Architecture

```
src/
├── main.rs, app.rs       # Entry, DI container (App holds all services)
├── cli/commands/         # Command handlers (16 commands)
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

## Key Types

- `SymbolKind`: function, class, method, field, struct, enum, interface, module, property, constructor, variable, constant, enum_member, type_parameter
- `Language`: 36 languages with aliases (e.g., `typescript`/`ts`, `python`/`py`)
- Location format: `file:line:column` (all 1-indexed in CLI)
