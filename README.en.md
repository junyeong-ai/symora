<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/symora_black.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/symora_white.png">
  <img alt="Symora" src="assets/symora_black.png" width="400">
</picture>

# Symora

**LSP-based Code Intelligence CLI for AI Coding Agents**

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)

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
| Text search | ✅ | ✅ ripgrep |
| AST search | ❌ | ✅ tree-sitter |

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
symora find impl src/main.rs:10:5                # find implementations
symora hover src/main.rs:10:5                    # type/doc info
symora calls incoming src/main.rs:10:5           # find callers
symora rename src/main.rs:10:5 new_name          # rename symbol
symora impact src/main.rs:10:5                   # impact analysis
symora diagnostics src/main.rs                   # LSP diagnostics
```

### Code Search
```bash
symora search text "TODO" --type rust            # ripgrep-based
symora search ast "function_item" --lang rust    # tree-sitter AST
symora search nodes --lang rust                  # list node types
```

> **Location format**: `file:line:column` (1-indexed)
> **`--limit 0`**: unlimited results

---

## Supported Languages (36)

Rust, TypeScript, Python, Go, Java, Kotlin, C++, C#, Swift, Ruby, PHP, Haskell, and more

```bash
symora doctor  # check installed language servers
```

---

## Configuration

```bash
symora config init           # project config (.symora/config.toml)
symora config init --global  # global config
```

---

## Troubleshooting

```bash
symora doctor           # check dependencies
symora daemon restart   # restart daemon
symora daemon status    # check daemon status
```

| Issue | Solution |
|-------|----------|
| LSP timeout | `symora daemon restart` |
| Kotlin no methods | `symora search ast "function_declaration" --lang kotlin` |
| Python slow on large project | Use AST search or wait |

---

## Links

- [GitHub](https://github.com/junyeong-ai/symora)
- [Developer Guide](CLAUDE.md)
