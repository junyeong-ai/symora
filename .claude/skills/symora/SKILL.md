---
name: symora
version: 1.1.0
description: Navigates and analyzes code semantically using Language Server Protocol with 30+ language support. Use when asked to go to definition, find references/usages, show who calls this function, trace call hierarchy, rename symbol across files, get type info via hover, list symbols in file, search by symbol name, perform BM25 ranked search (symbols/content), or perform AST-based code search (tree-sitter). Supports Rust, Go, Java, TypeScript, Python, C/C++, Kotlin, PHP. Prefer over grep/ripgrep for semantic code queries.
allowed-tools: Bash
---

# symora

LSP-based code intelligence CLI. **All output is JSON** — pipe through `jq` for extraction.

## Quick Command Reference

| User Request | Command |
|--------------|---------|
| Where is X defined? | `symora find def file:line:col` |
| Find all usages of X | `symora find refs file:line:col` |
| Find usages with code | `symora find refs file:line:col --with-snippet` |
| Who calls this function? | `symora calls incoming file:line:col` |
| What does this call? | `symora calls outgoing file:line:col` |
| Rename symbol across files | `symora rename file:line:col new_name` |
| Get type/docs info | `symora hover file:line:col` |
| List symbols in file | `symora find symbol file.rs` |
| Search by symbol name | `symora find symbol --name "Config" --lang rust` |
| Search symbols (BM25 ranked) | `symora search symbols "Handler" --kind class` |
| Search code content (BM25) | `symora search content "async fn" --lang rust` |
| Build search index | `symora search index build` |
| Find most used symbols | `symora usage "pattern" --lang rust --sort references` |
| Find undocumented symbols | `symora usage "*" --lang rust --filter no-docs` |
| Find untested symbols | `symora usage "*" --lang rust --filter no-tests` |
| Find dead code | `symora usage "*" --lang rust --filter zero-refs` |
| Find important symbols | `symora usage "*" --lang rust --min-refs 5` |
| Get error context for AI | `symora diagnostics file --with-context` |
| Extract method/variable | `symora actions list file:line:col --kind refactor` |
| Pattern-based edit | `symora edit pattern file --pattern "func" --lang rust --text "..."` |
| Gather all context | `symora context file:line:col --all` |
| Gather related context | `symora context file:line:col --callers --callees` |
| Analyze git diff impact | `symora diff-impact --callers` |
| Analyze staged changes | `symora diff-impact --staged --callers` |
| Batch refs lookup | `symora batch refs loc1 loc2 loc3` |

**Location format**: `file:line:column` (all 1-indexed)

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

# Find references with source code snippets
symora find refs src/main.rs:10:5 --with-snippet | jq '.references[] | {file, line, snippet}'

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

# Context gathering: related code for AI analysis
symora context src/main.rs:42:5 --all | jq '.'           # All context (callers, callees, types, tests)
symora context src/main.rs:42:5 --callers --callees | jq '.callers, .callees'
symora context src/main.rs:42:5 --types --tests | jq '.types, .tests'

# Diff impact: analyze changes in git diff
symora diff-impact | jq '.changes[]'                      # Changes against HEAD
symora diff-impact main | jq '.changes[]'                 # Changes against main branch
symora diff-impact --staged | jq '.changes[]'             # Staged changes only
symora diff-impact --callers | jq '.changes[] | {name, callers}'  # Include callers

# Batch operations: multiple locations at once
symora batch refs loc1 loc2 loc3 | jq '.results[]'
symora batch refs loc1 loc2 --with-snippet | jq '.results[].references[].snippet'
symora batch refs loc1 loc2 --parallel --fail-fast | jq '.results[]'
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
# BM25 ranked symbol search (SQLite FTS5)
symora search symbols "execute" | jq '.results[] | {name, file, line, score}'
symora search symbols "Handler" --kind class | jq '.results[].name'
symora search symbols "process" --limit 10 | jq '.results[]'

# BM25 ranked content search
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
| `symbols` | `--kind`, `--limit` | BM25 ranked, requires index |
| `content` | `--lang`, `--limit` | BM25 ranked, requires index |
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

# Function signature
symora signature src/main.rs:10:5 | jq '.signatures[0]'
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
| `--min-refs N` | Minimum references filter (find important symbols) | - |
| `--max-symbols N` | Max symbols to analyze | 50 |
| `--limit N` | Max results to display | 10 |

### 8. Refactoring Actions

```bash
# List all available code actions at location
symora actions list src/main.rs:10:5 | jq '.actions[]'

# Filter by action kind (refactor, quickfix, source)
symora actions list src/main.rs:10:5 --kind refactor | jq '.actions[].title'

# Apply a specific action by title
symora actions apply src/main.rs:10:5 "Extract method" | jq '.changes'

# Common refactoring actions (availability depends on LSP server)
# - Extract method/function
# - Extract variable/constant
# - Inline variable
# - Convert to async/await
# - Generate impl block
```

### 9. Pattern Edit (Structural)

```bash
# Edit code by tree-sitter AST pattern
symora edit pattern src/main.rs --pattern "function_item" --lang rust --text "// DEPRECATED\n{match}"

# {match} placeholder is replaced with the matched code
# Example: Add deprecation comment to all functions

# Standard position-based edit
symora edit src/main.rs:10:5 --old "old_text" --new "new_text"

# Edit by symbol path
symora edit symbol src/main.rs --symbol "Config/new" --text "fn new() -> Self { Self::default() }"
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
| `--with-context` | Include surrounding code context (diagnostics) |
| `--with-suggestions` | Include fix suggestions (diagnostics) |
| `--with-metrics` | Include usage metrics (usage) |
| `--filter` | Filter results: has-tests, no-tests, has-docs, no-docs, not-test-file, zero-refs (usage) |
| `--min-refs N` | Minimum references filter (usage) |
| `--max-symbols N` | Limit symbols to analyze for performance (usage, default: 50) |
| `--pattern` | tree-sitter AST pattern for structural edit |
| `--text` | Replacement text with `{match}` placeholder (edit pattern) |
| `--with-snippet` | Include source code snippet (find refs, batch, usage) |
| `--parallel` | Execute batch operations in parallel |
| `--fail-fast` | Stop batch on first failure |
| `--callers` | Include callers in context |
| `--callees` | Include callees in context |
| `--types` | Include type definitions in context |
| `--tests` | Include related tests in context |

## LSP Support Matrix

| Feature | Rust | Go | Java | TS/JS | Kotlin | Python | PHP | C/C++ |
|---------|:----:|:--:|:----:|:-----:|:------:|:------:|:---:|:-----:|
| find symbol | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ |
| find def | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ | ✅ | ✅ |
| find refs | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ✅ | ✅ |
| hover | ✅ | ✅ | ✅ | ⚠️ | ✅ | ⚠️ | ✅ | ✅ |
| calls | ✅ | ✅ | ✅ | ⚠️ | ❌ | ❌ | ❌ | ⚠️ |
| rename | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ❌ | ❌ | ✅ |
| actions | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ⚠️ | ✅ |
| usage | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ✅ | ✅ |
| search (BM25) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

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

## Advanced Patterns

```bash
# BM25 ranked search - find relevant code quickly
symora search symbols "Handler" --kind class | jq '.results[] | {name, file, score}'
symora search content "error" --lang rust --limit 50 | jq '.results[] | "\(.file):\(.line) \(.content)"'

# Build index for new project
symora search index build --force | jq '.stats'

# Documentation coverage analysis
symora usage "*" --lang rust --filter no-docs --with-metrics | jq '.results[] | {name, file, line}'

# Find most referenced symbols (hot spots)
symora usage "*" --lang rust --sort references --limit 20 | jq '.results[] | {name, refs: .metrics.references}'

# Find untested symbols in non-test files
symora usage "*" --lang rust --filter not-test-file | jq '[.results[] | select(.metrics.has_tests == false)] | length'

# Error context with fix suggestions
symora diagnostics src/main.rs --with-context --with-suggestions | jq '.diagnostics[] | {message, severity, context, suggestions}'

# Check available refactoring at cursor
symora actions list src/main.rs:10:5 --kind refactor | jq '.actions[].title'

# Add deprecation comment to all functions
symora edit pattern src/main.rs --pattern "function_item" --lang rust --text "/// @deprecated\n{match}"

# Check test coverage for symbols
symora usage "Handler" --lang rust --with-metrics | jq '.results[] | {name, has_tests: .metrics.has_tests, test_files: .metrics.test_files}'

# Find dead code (zero references)
symora usage "*" --lang rust --filter zero-refs | jq '.results[] | {name, file}'

# Find important symbols (5+ references)
symora usage "*" --lang rust --min-refs 5 --with-metrics | jq '.results[] | {name, refs: .metrics.references}'

# Batch refs lookup with snippets for multiple locations
symora batch refs src/main.rs:10:5 src/lib.rs:20:3 --with-snippet | jq '.results[].references[]'

# Context gathering for AI analysis
symora context src/main.rs:42:5 --callers --callees --types | jq '{callers, callees, types}'

# Combine context with tests for comprehensive view
symora context src/main.rs:42:5 --callers --tests | jq '{callers: .callers[].name, tests: .tests[]}'
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
| Usage search slow | Use `--max-symbols 30` to reduce scope |
| No refactoring actions | LSP may not support at that location |
| Pattern edit no matches | Check AST node type with `symora search nodes` |
| BM25 search no results | Run `symora search index build` first |
| Index build fails | Check disk space, run `symora search index clear` |
