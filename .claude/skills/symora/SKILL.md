---
name: symora
version: 0.5.0
description: Navigates and analyzes code semantically using Language Server Protocol with 36 language support. Use when asked to go to definition, find references/usages, show who calls this function, trace call hierarchy, rename symbol across files, get type info via hover, list symbols in file, search by symbol name, perform code search (symbols/content), or perform AST-based code search (tree-sitter). Supports Rust, Go, Java, TypeScript, Python, C/C++, Kotlin, PHP, TOML. Prefer over grep/ripgrep for semantic code queries.
argument-hint: "[file:line:col] or [command] [args]"
allowed-tools: Bash(symora*)
---

# symora

LSP-based code intelligence CLI for AI coding agents. **All output is JSON** — pipe through `jq` for extraction.

## Quick Command Reference

| User Request | Command |
|--------------|---------|
| Where is X defined? | `symora def file:line:col` |
| Find all usages of X | `symora refs file:line:col` |
| Find usages with code | `symora refs file:line:col --snippet` |
| Who calls this function? | `symora callers file:line:col` |
| What does this call? | `symora callees file:line:col` |
| Rename symbol across files | `symora rename file:line:col new_name` |
| Get type/docs info | `symora hover file:line:col` |
| List symbols in file | `symora symbols file.rs` |
| Search by symbol name | `symora symbols --name "Config" --lang rust` |
| Search symbols | `symora search symbols "Handler" --kind class` |
| Search code content | `symora search content "async fn" --lang rust` |
| Build search index | `symora search index build` |
| Find most used symbols | `symora usage "pattern" --lang rust --sort references` |
| Find undocumented symbols | `symora usage "*" --lang rust --filter no-docs` |
| Find untested symbols | `symora usage "*" --lang rust --filter no-tests` |
| Find dead code | `symora usage "*" --lang rust --filter zero-refs` |
| Get error context for AI | `symora diagnostics file --with-context` |
| Gather all context | `symora context file:line:col --all` |
| Analyze git diff impact | `symora diff-impact --callers` |
| List code actions | `symora actions list file:line:col` |
| Apply code action | `symora actions apply file:line:col "title"` |
| Find parent types | `symora supertypes file:line:col` |
| Find child types | `symora subtypes file:line:col` |
| Get function signature | `symora signature file:line:col` |
| Diagnose environment | `symora doctor` |

**Location format**: `file:line:column` (all 1-indexed)

## Global Options

| Option | Description |
|--------|-------------|
| `-c, --compact` | Compact output (single-line JSON, saves tokens for AI tools) |
| `-q, --quiet` | Quiet mode (errors only, no output on success) |
| `-v, --verbose` | Enable debug logging |

```bash
symora -c refs src/main.rs:10:5       # AI-friendly compact output
symora -q rename src/main.rs:10:5 foo # No output on success
symora -v status                      # Debug logs
```

## Core Workflows

### 1. Understand Code Structure

```bash
# Find all symbols in file
symora symbols src/main.rs | jq '.items[] | {name, kind, line: .location.line}'

# Find specific kind
symora symbols src/main.rs --kind function | jq '.items[].name'

# Find Rust traits (alias for interface)
symora symbols src/main.rs --kind trait | jq '.items[].name'

# Find by name across workspace
symora symbols --name "Config" --lang rust | jq '.items[]'

# Get type info and documentation
symora hover src/main.rs:10:5 | jq -r '.content'
```

### 2. Navigate Code

```bash
# Go to definition
symora def src/main.rs:10:5 | jq '.definition'

# Find all references
symora refs src/main.rs:10:5 | jq '.items[] | "\(.file):\(.line)"'

# Find references with source code snippets
symora refs src/main.rs:10:5 --snippet | jq '.items[] | {file, line, snippet}'

# Find implementations of trait/interface
symora impl src/main.rs:10:5 | jq '.items[]'

# Chain: definition → references
def=$(symora def src/main.rs:10:5 | jq -r '"\(.definition.file):\(.definition.line):\(.definition.column)"')
symora refs "$def" | jq '.count'
```

### 3. Analyze Call and Type Hierarchy

```bash
# Who calls this function?
symora callers src/main.rs:42:5 | jq '.items[] | {name, file: .location.file, line: .location.line}'

# What does this function call?
symora callees src/main.rs:42:5 | jq '.items[].name'

# Disable auto refs-fallback when call hierarchy unsupported
symora callers src/main.rs:42:5 --no-fallback | jq '.items[]'

# Find parent types (superclasses, interfaces)
symora supertypes src/main.rs:10:5 | jq '.items[] | {name, file}'

# Find child types (subclasses, implementations)
symora subtypes src/main.rs:10:5 | jq '.items[] | {name, file}'

# Get function signature at call site
symora signature src/main.rs:42:10 | jq '.signatures[] | {label, parameters}'

# Impact analysis: files affected by changes
symora impact src/main.rs:42:5 | jq '.affected_files[]'

# Context gathering: related code for AI analysis
symora context src/main.rs:42:5 --all | jq '.'           # All context (callers, callees, types, tests)
symora context src/main.rs:42:5 --callers --callees | jq '.callers, .callees'
symora context src/main.rs:42:5 --types --tests | jq '.types, .tests'

# Diff impact: analyze changes in git diff
symora diff-impact | jq '.changes[]'                      # Changes against HEAD
symora diff-impact main | jq '.changes[]'                 # Changes against main branch
symora diff-impact --staged | jq '.changes[]'             # Staged changes only
symora diff-impact --callers | jq '.changes[] | {name, callers}'  # Include callers
```

### 4. Refactor Code

```bash
# Rename symbol (preview first)
symora rename src/main.rs:10:5 new_name --dry-run | jq '.changes[]'

# Apply rename
symora rename src/main.rs:10:5 new_name | jq '.changes | length'

# List available code actions (quickfix, refactor, etc.)
symora actions list src/main.rs:10:5 | jq '.items[] | {title, kind}'

# List only quickfix actions
symora actions list src/main.rs:10:5 --kind quickfix | jq '.items[].title'

# Preview action (dry-run)
symora actions apply src/main.rs:10:5 "Extract" --dry-run | jq '.'

# Apply code action by title (partial match)
symora actions apply src/main.rs:10:5 "Extract function" | jq '.changes[]'

# Edit symbol body by path
symora edit symbol src/main.rs --symbol "Config/new" --text "fn new() -> Self { Self::default() }"

# Insert code after symbol
symora edit insert-after src/main.rs --symbol "Config" --text "\nimpl Default for Config { ... }"

# Insert code before position
symora edit insert-before src/main.rs:10:5 --text "// comment\n"
```

### 5. Search Code

```bash
# Symbol search (SQLite LIKE, supports substring matching)
symora search symbols "execute" | jq '.results[] | {name, file, line}'
symora search symbols "Handler" --kind class | jq '.results[].name'
symora search symbols "process" --limit 10 | jq '.results[]'

# Content search (SQLite LIKE)
symora search content "async fn" | jq '.results[] | "\(.file):\(.line)"'
symora search content "TODO" --lang rust | jq '.results[].content'
symora search content "error handling" --limit 20 | jq '.results[]'

# Search index management
symora search index build                    # Build/update index
symora search index build --force            # Force full rebuild
symora search index build --lang rust,python # Specific languages
symora search index status | jq '.'          # Index stats
symora search index clear                    # Clear index

# AST search (tree-sitter) - 13 languages
# Python, TypeScript/TSX, JavaScript, Rust, Go, Java, Kotlin, C++, C#, Bash, Ruby, Lua, PHP
symora search ast "function_item" --lang rust | jq '.matches[].text'
symora search ast "class_declaration" --lang csharp | jq '.matches[].text'

# List available node types for a language
symora search nodes --lang typescript
```

| Search Type | Options | Notes |
|-------------|---------|-------|
| `symbols` | `--kind`, `--limit` | SQLite LIKE, requires index |
| `content` | `--lang`, `--limit` | SQLite LIKE, requires index |
| `ast` | `--lang` (required), `--path`, `--limit` | tree-sitter patterns |
| `index` | `build [--force] [--lang]`, `status`, `clear` | Stored at `.symora/search.db` |

### 6. Check Code Health

```bash
# Get diagnostics (errors, warnings)
symora diagnostics src/main.rs | jq '.diagnostics[] | "\(.severity): \(.message)"'

# AI-friendly diagnostics with surrounding code context
symora diagnostics src/main.rs --with-context | jq '.diagnostics[] | {message, context}'

# Diagnostics with fix suggestions
symora diagnostics src/main.rs --with-suggestions | jq '.diagnostics[] | {message, suggestions}'
```

### 7. Usage Analysis

```bash
# Search symbols by pattern with metrics
symora usage "process" --lang rust | jq '.results[]'

# Sort by reference count (most used first)
symora usage "Order" --lang rust --sort references | jq '.results[] | {name, file}'

# Include detailed metrics (ref count, has tests, has docs)
symora usage "Config" --lang rust --with-metrics | jq '.results[] | {name, metrics}'

# Find undocumented symbols (doc coverage analysis)
symora usage "*" --lang rust --filter no-docs | jq '.results[].name'

# Find symbols with tests
symora usage "*" --lang rust --filter has-tests | jq '.results[].name'

# Find symbols without tests (test coverage analysis)
symora usage "*" --lang rust --filter no-tests | jq '.results[].name'

# Find dead code (zero references)
symora usage "*" --lang rust --filter zero-refs | jq '.results[].name'

# Find important symbols (5+ references)
symora usage "*" --lang rust --min-refs 5 | jq '.results[]'

# Exclude test files from results
symora usage "*" --lang rust --filter not-test-file | jq '.results[]'

# Combine filters
symora usage "*" --lang rust --filter no-docs,not-test-file | jq '.count'

# Performance: limit symbols to analyze (default: 50)
symora usage "fn" --lang rust --max-symbols 100 --limit 20 | jq '.results[]'
```

| Option | Description | Default |
|--------|-------------|---------|
| `--lang` | Language filter (required) | - |
| `--sort references\|name` | Sort by reference count or name | references |
| `--filter` | has-tests, no-tests, has-docs, no-docs, not-test-file, zero-refs | - |
| `--with-metrics` | Include ref count, test/doc status | false |
| `--with-snippet` | Include code snippet | false |
| `--min-refs N` | Minimum references filter | - |
| `--max-symbols N` | Max symbols to analyze | 50 |
| `--limit N` | Max results to display | 10 |

### 8. Pattern Edit (Structural)

```bash
# Edit code by tree-sitter AST pattern
symora edit pattern src/main.rs --pattern "function_item" --lang rust --text "// DEPRECATED\n{match}"

# {match} placeholder is replaced with the matched code

# Standard position-based edit
symora edit replace src/main.rs:10:5 --text "new_text"

# Edit by symbol path
symora edit symbol src/main.rs --symbol "Config/new" --text "fn new() -> Self { Self::default() }"
```

## Key Options

**Global Options** (apply to all commands):

| Option | Description |
|--------|-------------|
| `-c, --compact` | Compact output (single-line JSON, saves tokens) |
| `-q, --quiet` | Quiet mode (errors only) |
| `-v, --verbose` | Enable debug logging |

**Command Options**:

| Option | Description |
|--------|-------------|
| `--kind` | function, class, method, struct, enum, interface, trait, field, variable, constant |
| `--limit N` | Max results (0 = unlimited) |
| `--dry-run` | Preview changes without applying |
| `--depth N` | Include nested symbols |
| `--body` | Include symbol source code |
| `--with-context` | Include surrounding code context (diagnostics) |
| `--with-suggestions` | Include fix suggestions (diagnostics) |
| `--with-metrics` | Include usage metrics (usage) |
| `--snippet` | Include source code snippet (refs, usage) |
| `--no-fallback` | Disable auto refs-fallback (callers) |

## LSP Support Matrix

| Feature | Rust | Go | Java | TS/JS | Kotlin | Python | PHP | C/C++ |
|---------|:----:|:--:|:----:|:-----:|:------:|:------:|:---:|:-----:|
| symbols | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ |
| def | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ✅ | ✅ |
| refs | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ✅ | ✅ |
| hover | ✅ | ✅ | ✅ | ⚠️ | ✅ | ⚠️ | ✅ | ✅ |
| callers/callees | ✅ | ✅ | ✅ | ⚠️ | ❌* | ❌* | ❌* | ⚠️ |
| supertypes/subtypes | ❌ | ❌ | ✅ | ✅ | ❌ | ❌ | ❌ | ✅ |
| rename | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ❌ | ❌ | ✅ |
| usage | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ✅ | ✅ |
| search | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

Legend: ✅ Full | ⚠️ Limited/Slow | ❌ Not Supported | ❌* Auto refs-fallback available

## Language-Specific Notes

### Rust
- Best supported language with full LSP integration
- Use `--kind trait` for Rust traits (aliased to interface)

### Kotlin (JetBrains kotlin-lsp)
- **Document symbols**: Class-level only - methods NOT returned by LSP
- **Call hierarchy**: Not supported - auto refs-fallback used
- **Best practice**:
  ```bash
  # Classes via LSP
  symora symbols file.kt | jq '.items[]'

  # Methods via AST (recommended)
  symora search ast "function_declaration" --lang kotlin --path file.kt
  ```

### TypeScript/JavaScript
- Arrow functions: Use `--kind constant` (not `function`)
- **Initial requests may be slow** (15s+ on large monorepos)
- Call hierarchy: Partial support

### Python (pyright)
- **Large monorepos may timeout** - use AST search as fallback
  ```bash
  symora search ast "function_definition" --lang python --path src/
  ```
- Call hierarchy: Not supported - auto refs-fallback used

### TOML (taplo)
- Full LSP support for TOML files
- Hover, diagnostics, symbols available

## Troubleshooting

```bash
symora doctor           # Diagnose all language servers
symora doctor rust      # Check specific language
symora daemon restart   # Restart daemon (fixes most issues)
symora daemon status    # Check daemon status
symora -v <command>     # Enable debug logging
```

| Issue | Solution |
|-------|----------|
| LSP timeout | `symora daemon restart` |
| Empty results | Run `symora doctor` to check LSP server |
| Slow first request | Normal - LSP indexing (wait 10-30s) |
| Kotlin no methods | Use `symora search ast "function_declaration"` |
| Python timeout | Use AST search as fallback |
| Usage search slow | Use `--max-symbols 30` to reduce scope |
| Call hierarchy unsupported | Auto refs-fallback (or use `--no-fallback` to disable) |
| Search no results | Run `symora search index build` first |
