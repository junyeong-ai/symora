---
name: symora
version: 1.0.0
description: |
  LSP-powered code analysis CLI for multi-language projects. Use for: finding definitions,
  references, call hierarchies, symbol lookup, code navigation, rename refactoring, impact analysis.
  Triggers: find definition, find references, who calls, what calls, symbol search, rename symbol,
  analyze code, go to definition, find usages, callers, callees, code structure.
allowed-tools: Bash
---

# symora

LSP-based code intelligence. **All output is JSON** — use `jq` for extraction.

## Location Format

`file:line:column` (1-indexed)

## Core Workflows

### 1. Understand Code Structure

```bash
# Find all symbols in file
symora find symbol src/main.rs | jq '.symbols[] | {name, kind, line}'

# Find specific kind
symora find symbol src/main.rs --kind function | jq '.symbols[].name'

# Find Rust traits (alias for interface)
symora find symbol src/main.rs --kind trait | jq '.symbols[].name'

# Find by name across workspace
symora find symbol --name "Config" --lang rust | jq '.symbols[]'

# Get type info and documentation
symora hover src/main.rs:10:5 | jq -r '.content'
```

### 2. Navigate Code

```bash
# Go to definition
symora find def src/main.rs:10:5 | jq '.definition'

# Find all references
symora find refs src/main.rs:10:5 | jq '.references[] | "\(.file):\(.line)"'

# Find implementations of trait/interface
symora find impl src/main.rs:10:5 | jq '.references[]'

# Chain: definition → references
def=$(symora find def src/main.rs:10:5 | jq -r '"\(.definition.file):\(.definition.line):\(.definition.column)"')
symora find refs "$def" | jq '.count'
```

### 3. Analyze Call Hierarchy

```bash
# Who calls this function?
symora calls incoming src/main.rs:42:5 | jq '.calls[] | {name, file, line}'

# What does this function call?
symora calls outgoing src/main.rs:42:5 | jq '.calls[].name'

# Impact analysis: files affected by changes
symora impact src/main.rs:42:5 | jq '.affected_files[]'
```

### 4. Refactor Code

```bash
# Rename symbol (preview first)
symora rename src/main.rs:10:5 new_name --dry-run | jq '.changes[]'

# Apply rename
symora rename src/main.rs:10:5 new_name | jq '.changes | length'

# Edit symbol body by path
symora edit symbol src/main.rs --symbol "Config/new" --text "fn new() -> Self { Self::default() }"

# Insert code after symbol
symora edit insert-after src/main.rs --symbol "Config" --text "\nimpl Default for Config { ... }"
```

### 5. Search Code

```bash
# Text search (ripgrep) - all languages
symora search text "TODO" --type rust | jq '.matches[] | "\(.file):\(.line)"'

# AST search (tree-sitter) - 13 languages
# Python, TypeScript/TSX, JavaScript, Rust, Go, Java, Kotlin, C++, C#, Bash, Ruby, Lua, PHP
symora search ast "function_item" --lang rust | jq '.matches[].text'
symora search ast "class_declaration" --lang csharp | jq '.matches[].text'

# List available node types for a language
symora search nodes --lang typescript

# Unlimited results
symora search text "error" --type rust --limit 0 | jq '.count'
```

### 6. Check Code Health

```bash
# Get diagnostics (errors, warnings)
symora diagnostics src/main.rs | jq '.diagnostics[] | "\(.severity): \(.message)"'

# Function signature
symora signature src/main.rs:10:5 | jq '.signatures[0]'
```

## Symbol Path Filter

```bash
# Exact path
symora find symbol src/main.rs --symbol "MyClass/method"

# Wildcard: any parent
symora find symbol src/main.rs --symbol "*/update"

# Wildcard: all children
symora find symbol src/main.rs --symbol "MyClass/*"
```

## Key Options

| Option | Description |
|--------|-------------|
| `--kind` | function, class, method, struct, enum, interface, trait, field, variable, constant |
| `--limit N` | Max results (0 = unlimited) |
| `--dry-run` | Preview changes without applying |
| `--depth N` | Include nested symbols |
| `--body` | Include symbol source code |

## LSP Support Matrix

| Feature | Rust | Go | Java | TS/JS | Kotlin | Python | PHP | C/C++ |
|---------|:----:|:--:|:----:|:-----:|:------:|:------:|:---:|:-----:|
| find symbol | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ |
| find def | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ✅ | ✅ |
| find refs | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ✅ | ✅ |
| hover | ✅ | ✅ | ✅ | ⚠️ | ✅ | ⚠️ | ✅ | ✅ |
| calls | ✅ | ✅ | ✅ | ⚠️ | ❌ | ❌ | ❌ | ⚠️ |
| rename | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ❌ | ❌ | ✅ |

Legend: ✅ Full | ⚠️ Limited/Slow | ❌ Not Supported

## Language-Specific Notes

### Rust
- Best supported language with full LSP integration
- Use `--kind trait` for Rust traits (aliased to interface)

### Kotlin (JetBrains kotlin-lsp)
- Uses official JetBrains Kotlin Language Server (pre-alpha)
- **Document symbols**: Class-level only - methods NOT returned by LSP
- **Call hierarchy**: Not supported - use `find refs` instead
- **Best practice for Kotlin**:
  ```bash
  # Classes via LSP
  symora find symbol file.kt | jq '.symbols[]'

  # Methods via AST (recommended)
  symora search ast "function_declaration" --lang kotlin --path file.kt

  # Properties via AST
  symora search ast "property_declaration" --lang kotlin --path file.kt

  # Workspace search (alternative)
  symora find symbol --name "methodName" --lang kotlin | jq '.symbols[]'
  ```

### TypeScript/JavaScript
- Arrow functions: Use `--kind constant` (not `function`)
  - `const fn = () => {}` is a constant declaration per LSP spec
- **Initial requests may be slow** (15s+ on large monorepos) - subsequent requests are fast
- Call hierarchy: Partial support (may return empty on some projects)

### C/C++ (clangd)
- C structs: Use `--kind class` (clangd maps struct → class)
- Or use `--kind struct` which also matches class

### Python (pyright)
- **Large monorepos may timeout** - pyright needs extended indexing time
- Fallback: Use AST search for comprehensive function discovery
  ```bash
  symora search ast "function_definition" --lang python --path src/
  ```
- Call hierarchy: Not reliably supported
- If timeouts persist: `symora daemon restart`

### PHP (intelephense)
- Document symbols: Top-level only (like Kotlin)
- Use AST search for comprehensive method discovery:
  ```bash
  symora search ast "(method_declaration)" --lang php
  ```

### C# (csharp-ls)
- Requires: `dotnet tool install -g csharp-ls`
- Without LSP: Only AST search and text search work

## Common Patterns

```bash
# Find where a function is defined and all its callers
loc=$(symora find symbol src/main.rs --symbol "*/process" | jq -r '.symbols[0] | "\(.file):\(.line):\(.column)"')
symora calls incoming "$loc" | jq '.calls[]'

# Check if rename is safe
symora rename src/main.rs:10:5 new_name --dry-run | jq '.changes | length'

# Get function signature before editing
symora hover src/main.rs:10:5 | jq -r '.content'

# Kotlin: find methods via workspace search
symora find symbol --name "execute" --lang kotlin | jq '.symbols[]'

# C: find structs (mapped to class by clangd)
symora find symbol main.c --kind class | jq '.symbols[].name'

# Python: find all function definitions
symora search ast "(function_definition)" --lang python | jq '.matches | length'
```

## Troubleshooting

```bash
# Check LSP server status
symora doctor

# Restart daemon (fixes most LSP issues)
symora daemon restart

# Check daemon status
symora daemon status
```

| Issue | Solution |
|-------|----------|
| LSP timeout | `symora daemon restart` |
| Empty results | Check `symora doctor` for LSP server |
| Slow first request | Normal - LSP indexing (wait 10-30s) |
| Kotlin no methods | Use `symora search ast "function_declaration"` |
| Python timeout | Use AST search as fallback |
| Invalid regex error | Check regex syntax (ripgrep regex) |
