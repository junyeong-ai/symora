<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/symora_black.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/symora_white.png">
  <img alt="Symora" src="assets/symora_black.png" width="400">
</picture>

# Symora

**LSP-based Code Intelligence CLI for AI Coding Agents**

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
[![DeepWiki](https://img.shields.io/badge/DeepWiki-junyeong--ai%2Fsymora-blue.svg?logo=data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACwAAAAyCAYAAAAnWDnqAAAAAXNSR0IArs4c6QAAA05JREFUaEPtmUtyEzEQhtWTQyQLHNak2AB7ZnyXZMEjXMGeK/AIi+QuHrMnbChYY7MIh8g01fJoopFb0uhhEqqcbWTp06/uv1teleaEDv4O3n3dV60RfP947Mm9/SQc0teleIFQgzfc4CYZoTPAswgSJCCUJUnAAoRHOAUOcATwbmVLWdGoH//PB8mnKqScAhsD0kYP3j/Yt5LPQe2KvcXmGvRHcDnpxfL2zOYJ1mFwrryWTz0advv1Ut4CJgf5teleuhDuDj5eUcAUoahrdY/56teleebRWeraTjMt/00Sh3UDtjgHtQNHwcRGOC98teleJEAEymycmYcWwOprTgcB6VZ5JK5TAJ+fXGLBm3FDAmn6oPPjR4rKCAoJCal2eAiQp2x0vxTPB3ALO2CRkwmDy5WohzBDwSEFKRwPbknEggCPB/imwrycgxX2NzoMCHhPkDwqYMr9tRcP5qNrMZHkVnOjRMWwLCcr8ohBVb1OMjxLwGCvjTikrsBOiA6fNyCrm8V1rP93iVPpwaE+gO0SsWmPiXB+jikdf6SizrT5qKasx5j8ABbHpFTx+vFXp9EnYQmLx02h1QTTrl6eDqxLnGjporxl3NL3agEvXdT0WmEost648sQOYAeJS9Q7bfUVoMGnjo4AZdUMQku50McDcMWcBPvr0SzbTAFDfvJqwLzgxwATnCgnp4wDl6Aa+Ax283gghmj+vj7feE2KBBRMW3FzOpLOADl0Isb5587h/U4gGvkt5v60Z1VLG8BhYjbzRwyQZemwAd6cCR5/XFWLYZRIMpX39AR0tjaGGiGzLVyhse5C9RKC6ai42ppWPKiBagOvaYk8lO7DajerabOZP46Lby5wKjw1HCRx7p9sVMOWGzb/vA1hwiWc6jm3MvQDTogQkiqIhJV0nBQBTU+3okKCFDy9WwferkHjtxib7t3xIUQtHxnIwtx4mpg26/HfwVNVDb4oI9RHmx5WGelRVlrtiw43zboCLaxv46AZeB3IlTkwouebTr1y2NjSpHz68WNFjHvupy3q8TFn3Hos2IAk4Ju5dCo8B3wP7VPr/FGaKiG+T+v+TQqIrOqMTL1VdWV1DdmcbO8KXBz6esmYWYKPwDL5b5FA1a0hwapHiom0r/cKaoqr+27/XcrS5UwSMbQAAAABJRU5ErkJggg==)](https://deepwiki.com/junyeong-ai/symora)

**English** | [한국어](README.md)

---

## The Name

**Sym** (Symbol) + **ora** (Latin: boundary, gate)

An analysis tool that deciphers code's symbol structure and opens the gate to relationships across file and module boundaries.

---

## Background

Inspired by [Serena](https://github.com/oraios/serena).

| | Serena | Symora |
|---|--------|--------|
| Design Philosophy | Framework integration | CLI-first |
| Interface | MCP server | Bash commands |
| Language | Python | Rust |

Run `symora find refs src/main.rs:10:5` right after installation — instant integration with Claude Code skills or shell-based AI agents.

---

## Why Symora?

grep finds text. **Symora analyzes code structure through LSP.**

```bash
# grep: text pattern matching
grep -r "processOrder" .

# Symora: LSP-based code analysis
symora find refs src/order.rs:42:5       # all locations referencing this symbol
symora find def src/api.rs:15:10         # symbol definition location
symora hover src/api.rs:15:10            # type info and documentation
symora calls incoming src/order.rs:42:5  # call hierarchy analysis
```

| Feature | grep/ripgrep | Symora |
|---------|--------------|--------|
| Go to definition | ❌ | ✅ LSP |
| Find references | ❌ | ✅ LSP |
| Type information | ❌ | ✅ LSP |
| Call hierarchy | ❌ | ✅ LSP |
| Rename refactoring | ❌ | ✅ LSP |
| Code search | ❌ | ✅ SQLite |
| AST search | ❌ | ✅ tree-sitter |
| Usage metrics | ❌ | ✅ LSP |
| Doc coverage | ❌ | ✅ LSP |
| Pattern edit | ❌ | ✅ tree-sitter |
| Git Diff Impact | ❌ | ✅ LSP |

---

## Supported Platforms

| Platform | Support | Notes |
|----------|:-------:|-------|
| Linux (x86_64, aarch64) | ✅ | Full features |
| macOS (Apple Silicon) | ✅ | Full features |
| Windows | ❌ | Unix socket dependency |

> Symora uses Unix domain sockets for daemon IPC. Windows support is not planned.

---

## Quick Start

```bash
cargo install --path .
symora doctor          # check language servers
symora find symbol src/main.rs
```

---

## Core Features

### LSP-based Analysis
```bash
symora find symbol src/main.rs --kind function   # symbol discovery
symora find def src/main.rs:10:5                 # go to definition
symora find refs src/main.rs:10:5                # find references
symora find refs src/main.rs:10:5 --with-snippet # references + source code
symora find impl src/main.rs:10:5                # find implementations
symora hover src/main.rs:10:5                    # type/doc info
symora calls incoming src/main.rs:10:5           # find callers
symora calls incoming src/main.rs:10:5 --no-fallback  # disable fallback
symora rename src/main.rs:10:5 new_name          # rename symbol
symora impact src/main.rs:10:5                   # impact analysis
symora diagnostics src/main.rs                   # LSP diagnostics
symora diagnostics src/main.rs --with-context    # include AI-friendly context
symora diagnostics src/main.rs --with-suggestions # include fix suggestions
```

### Usage Finder
```bash
symora usage "process" --lang rust               # search symbols by pattern
symora usage "Order" --lang rust --sort references     # sort by reference count
symora usage "Config" --lang rust --with-metrics # include detailed metrics
symora usage "*" --lang rust --filter no-docs    # find undocumented symbols
symora usage "*" --lang rust --filter no-tests   # find untested symbols
symora usage "*" --lang rust --filter zero-refs  # dead code detection
symora usage "*" --lang rust --min-refs 5        # important symbols (5+ refs)
symora usage "fn" --lang rust --with-snippet     # include code snippet
```

| Option | Description |
|--------|-------------|
| `--sort references\|name` | Sort criteria (default: references) |
| `--filter` | has-tests, no-tests, has-docs, no-docs, not-test-file, zero-refs |
| `--with-metrics` | Show reference count, test status, doc status |
| `--with-snippet` | Include source code snippet |
| `--min-refs N` | Minimum references filter (find important symbols) |
| `--max-symbols N` | Max symbols to analyze (default: 50) |
| `--limit N` | Limit output results (default: 10) |

### Refactoring
```bash
symora actions list src/main.rs:10:5              # list available code actions
symora actions list src/main.rs:10:5 --kind refactor  # refactoring only
symora actions apply src/main.rs:10:5 "Extract..."    # apply action
```

### Context Gathering
```bash
symora context src/main.rs:10:5 --all                # all context (callers, callees, types, tests)
symora context src/main.rs:10:5 --callers --callees  # callers/callees
symora context src/main.rs:10:5 --types --tests      # type definitions, related tests
```

### Git Diff Impact Analysis
```bash
symora diff-impact                        # compare against HEAD
symora diff-impact main                   # compare against main branch
symora diff-impact --staged               # analyze staged changes only
symora diff-impact --callers              # include caller analysis
symora diff-impact --max-symbols 30       # limit symbols to analyze
```

| Output | Description |
|--------|-------------|
| `changed_symbols_count` | Number of changed symbols |
| `test_coverage.coverage_ratio` | Test coverage ratio |
| `changes[].reference_count` | Reference count per symbol |
| `changes[].callers` | Caller list (--callers option) |

### Batch Processing
```bash
symora batch refs loc1 loc2 loc3                    # batch lookup multiple locations
symora batch refs loc1 loc2 --with-snippet          # include snippets
symora batch refs loc1 loc2 --parallel --fail-fast  # parallel execution, stop on failure
```

### Pattern Edit (Structural)
```bash
symora edit pattern src/main.rs --pattern "(struct_item)" --lang rust --text "// NEW" --dry-run
symora edit replace src/main.rs:10:1 --text "new code" --dry-run
symora edit insert-after src/main.rs --symbol "MyFunc" --text "// comment" --dry-run
symora edit insert-before src/main.rs:10:5 --text "// comment" --dry-run
```

### Code Search
```bash
# Symbol/content search (SQLite LIKE-based, supports substring matching)
symora search symbols "execute" --kind function  # symbol search
symora search symbols "Handler" --limit 10       # limit results
symora search content "async fn" --lang rust     # content search
symora search content "TODO" --limit 20          # limit results

# Search index management
symora search index build                        # build index
symora search index build --force --lang rust    # force rebuild
symora search index status                       # index status
symora search index clear                        # clear index

# AST Search (tree-sitter)
symora search ast "function_item" --lang rust    # structural search
symora search nodes --lang rust                  # list node types
```

| Search Type | Purpose | Engine |
|-------------|---------|--------|
| `symbols` | Symbol name search | SQLite |
| `content` | Code line search | SQLite |
| `ast` | Structural pattern search | tree-sitter |

> **Location format**: `file:line:column` (1-indexed)
> **`--limit 0`**: unlimited results

---

## Supported Languages (36)

Rust, TypeScript, Python, Go, Java, Kotlin, C++, C#, Swift, Ruby, PHP, Haskell, TOML, and more

```bash
symora doctor  # check installed language servers
```

---

## Configuration

```bash
symora config init           # project config (.symora/config.toml)
symora config init --global  # global config
```

Key settings:
```toml
[lsp]
timeout_secs = 60       # LSP timeout (default: 60s)
refs_limit = 500        # Reference results limit
calls_limit = 100       # Call hierarchy limit
tests_limit = 10        # Test results limit

[test]
file_patterns = ["_check.rs"]  # Custom test file patterns
dir_patterns = ["/verification/"]
markers = ["@MyTest"]
```

---

## Troubleshooting

```bash
symora doctor           # check dependencies
symora daemon restart   # restart daemon
symora daemon status    # check daemon status
symora -v <command>     # enable debug logging
```

| Issue | Solution |
|-------|----------|
| LSP timeout | `symora daemon restart` |
| Kotlin no methods | `symora search ast "function_declaration" --lang kotlin` |
| Python slow on large project | Use AST search or wait |
| Usage search slow | Use `--max-symbols 30` to reduce analysis scope |
| Call hierarchy unsupported | Run without `--no-fallback` (auto refs fallback) |

---

## Links

- [GitHub](https://github.com/junyeong-ai/symora)
- [Developer Guide](CLAUDE.md)
