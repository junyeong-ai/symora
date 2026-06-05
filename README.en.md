<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/symora_black.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/symora_white.png">
  <img alt="Symora" src="assets/symora_black.png" width="400">
</picture>

# Symora

**Symbol-centric code intelligence CLI for AI coding agents**

[![Rust](https://img.shields.io/badge/rust-1.95%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)

**English** | [한국어](README.md)

---

## What Symora Is

Symora is a CLI-first code intelligence tool built for AI coding agents.

It combines:

- LSP-based semantic navigation
- SQLite-backed symbol and content search
- tree-sitter AST search
- a Unix daemon for reusable language-server sessions

Symora is designed for shell-driven workflows, structured JSON output, and exact follow-up from a symbol or location.

---

## Why Symora

Text search is useful, but agents often need semantic answers:

- what symbol is here?
- where is it referenced?
- what calls it?
- what file should I inspect next?
- what changed impact-wise?

Symora is built around those workflows.

```bash
# rough discovery
symora search symbols AuthUser

# inspect one file semantically
symora map file src/main.rs
symora symbols src/main.rs

# exact follow-up from a location
symora context src/main.rs:42 --all
symora refs src/main.rs:42
symora usage src/main.rs:42:10
```

---

## Core Capabilities

### Semantic Navigation

```bash
symora symbols src/main.rs
symora def src/main.rs:10:5
symora refs src/main.rs:10:5
symora hover src/main.rs:10:5
symora callers src/main.rs:10:5
symora callees src/main.rs:10:5
symora typedef src/main.rs:10:5
symora implementations src/main.rs:10:5
symora rename src/main.rs:10:5 new_name
```

### Search and Discovery

```bash
symora search symbols AuthUser
symora search content "async fn"
symora search ast "(function_item)" --lang rust
symora search nodes --lang rust
```

### Project and File Exploration

```bash
symora map summary
symora map file src/cli/commands/search.rs
symora map dir src/cli
symora map related src/cli/commands/search.rs
```

### Context and Usage Analysis

```bash
symora context src/main.rs:42 --all
symora refs src/main.rs:42
symora usage SearchCommand
symora usage src/cli/commands/search.rs:30:10
symora impact src/main.rs:42
symora diff-impact
```

### Edit and Refactor Support

```bash
symora actions list src/main.rs:42:5
symora actions apply src/main.rs:42:5 "Extract method"
symora edit replace src/main.rs:10:1 --text "new code" --dry-run
symora format src/main.rs
```

---

## Workflow Design

Symora works best when used in this order:

1. `symora map summary` for project entrypoints and major areas
2. `symora search symbols <query>` for rough workspace discovery
3. `symora map file <path>` for a compact file overview
4. `symora symbols <file>` or `symora symbols --symbol <path>` for exact inspection
5. `symora context`, `symora refs`, and `symora usage` for exact follow-up

This split is intentional:

- `search symbols` is for rough discovery
- `symbols` is for exact semantic inspection
- `map file` is a compact overview, not a full symbol dump

---

## Output Model

Symora prints JSON by default.

Important characteristics:

- project-relative paths when possible
- stable list-like fields such as `count`, `showing`, `items`, `truncated`, and `hints`
- compact mode for lower token usage

Global flags:

```bash
symora --format compact search symbols AuthUser   # compact JSON
symora -q refs src/main.rs:10:5     # errors only
symora -v status                    # debug logging
```

---

## Search Index

Symora includes a persistent SQLite-backed search index.

```bash
symora search index build
symora search index build --force --lang rust
symora search index status
symora search index clear
```

The index is stored under `.symora/store.db` in the current project.

Search commands also include fallback behavior when indexed or semantic features are unavailable, but the index is the most reliable path for repeated local use.

A normal `search index build` refreshes changed files and prunes files that no longer exist. Use `--force` only when you want a full rebuild.

---

## Configuration

Config precedence:

1. `.symora/config.toml`
2. `~/.config/symora/config.toml`
3. built-in defaults

Initialize config:

```bash
symora config init
symora config init --global
```

Common settings include:

- LSP timeouts and limits
- daemon behavior
- test file patterns
- ignored paths

---

## Platform and Runtime Notes

- Linux: supported
- macOS: supported
- Windows: not supported for daemon-based workflow because Symora uses Unix domain sockets

On Unix platforms, Symora uses a daemon by default for most commands and falls back to direct LSP execution where appropriate. `daemon start` and `daemon restart` launch the daemon in the background and return immediately.

Daemon commands:

```bash
symora daemon start
symora daemon stop
symora daemon restart
symora daemon status
```

---

## Installation

One-shot install (prebuilt binary, SHA-256 verified; prompts let you pick a source build and install the Claude Code skill — defaults apply without a TTY):

```bash
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh | bash
```

Useful variants:

```bash
# Pin a release / verify GitHub build provenance (needs gh CLI)
curl -fsSL .../install.sh | bash -s -- --version 0.9.0 --verify-attestations

# Source build + skill, no prompts (no checkout needed — builds the release tag from git)
curl -fsSL .../install.sh | bash -s -- --source --skill

# Non-interactive (CI): defaults to prebuilt, skill skipped
curl -fsSL .../install.sh | bash -s -- --prebuilt --no-skill
```

Platforms without a prebuilt (Intel Macs, etc.) fall back to a source build automatically (requires Rust). Inside a checkout, `./scripts/install.sh --source` builds the working tree, or:

```bash
cargo install --path .
```

```bash
# Change install location
curl -fsSL .../install.sh | SYMORA_INSTALL_DIR=/usr/local/bin bash
```

The binary owns the rest of its lifecycle:

```bash
symora setup                          # interactive: skill + language servers
symora setup skill                    # skill only
symora setup deps --group core       # dependencies only (core / core-jvm / core-web / core-systems / all)
symora self update                    # in-place upgrade to the latest release
symora self update --version 0.9.0   # pin a version
symora self uninstall                 # remove binary + skill + config + daemon data
symora self uninstall --keep-skill --keep-config
```

Check environment and language servers:

```bash
symora doctor
```

---

## MCP Server

Symora also runs as a Model Context Protocol server. The same command set is exposed as 21 MCP tools that share the in-process command layer, so both surfaces produce identical results.

```bash
symora mcp serve                          # stdio (Claude Code, Cursor, etc. use this by default)
symora mcp serve --transport http --port 8765
```

The tool list and input schemas are returned by `tools/list`. Mutating tools (`rename_symbol`, `apply_code_action`, `replace_symbol_body`, `insert_*`) carry `Mutates` in their descriptions and all support a `dry_run` option.

---

## Practical Notes

- `context`, `refs`, and `usage` accept exact locations such as `file:line:column`
- `usage` also accepts a location and resolves the symbol automatically
- `context` includes fallback guidance when the active LSP server does not support call hierarchy or type definition well
- semantic file/location commands require the language server for that language to be installed; check with `symora doctor <lang>` when in doubt
- `map related` is a heuristic helper for adjacent files, not a guaranteed dependency graph

---

## Repository Links

- [Developer guide](CLAUDE.md)
- [GitHub repository](https://github.com/junyeong-ai/symora)
