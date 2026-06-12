<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/symora_black.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/symora_white.png">
  <img alt="Symora" src="assets/symora_black.png" width="400">
</picture>

# Symora

**Symbol-centric code intelligence CLI for AI coding agents**

[![Rust](https://img.shields.io/badge/rust-1.96%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
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

- **Semantic navigation** — `symbols`, `def`, `refs`, `hover`, `callers`, `callees`, `typedef`, `implementations`
- **Search and discovery** — `search symbols`, `search content`, `search ast`, plus `pack` for a token-budgeted repo brief
- **Project and file exploration** — `map summary`, `map file`, `map dir`, `map related`
- **Context and impact analysis** — `context`, `usage`, `impact`, `diff-impact`
- **Edit and refactor** — `rename`, `actions`, the `edit` subcommands (symbol- or line-addressed splices with exact dry-run previews and a reference-guarded `delete`), `format`
- **Health checks** — `diagnostics`, `doctor`, `status`

Every command prints JSON; `--help` on any of them shows its flags and output shape.

---

## For AI Agents

The agent-facing playbook — workflow order, command selection, the output contract, failure handling — ships with the tool rather than this README, so it stays in lockstep with the binary:

- `symora setup skill` installs the Claude Code skill (the full CLI playbook).
- `symora mcp serve` returns the same guidance through the MCP `initialize` instructions.

The short version: list responses share one stable shape (`count`, `showing`, `items`, with disclosed `truncated`/`hints`/`next_commands`), positions are 1-indexed, failures are structured `{code, message, hint}`, and discovery flows from rough (`pack`, `map summary`, `search symbols`) to exact (`symbols`, `context`, `refs`, `impact`). Global flags such as `--format compact` (single-line JSON) and `-q` (errors only) may be placed before the subcommand.

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
- language-server launch overrides (`[lsp.servers.<lang>]`: command/args/tier)

```toml
[lsp.servers.typescript]
command = "/Users/me/.nvm/versions/node/v20.11.0/bin/typescript-language-server"
args = ["--stdio"]   # optional; absent = inherit builtin args
tier = "slow"        # optional; one of fast|standard|slow
```

Keys are the `language` ids printed by `symora doctor` — a rejected key is reported in doctor's `config_errors` and never applied. The daemon reads config at start; run `symora daemon restart` after changing it.

---

## Platform and Runtime Notes

- Linux: supported
- macOS: supported
- Windows: not supported for daemon-based workflow because Symora uses Unix domain sockets

On Unix, Symora uses a daemon by default (set `SYMORA_NO_DAEMON=1` for in-process direct execution). The mode is chosen once at startup; there is no runtime fallback. `daemon start` and `daemon restart` launch the daemon in the background and return immediately.

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
curl -fsSL .../install.sh | bash -s -- --version <version> --verify-attestations

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
symora self update --version <version>   # pin a version
symora self uninstall                 # remove binary + skill + config + daemon data
symora self uninstall --keep-skill --keep-config
```

Check environment and language servers:

```bash
symora doctor
```

---

## MCP Server

Symora also runs as a Model Context Protocol server. Its main navigation, analysis, and edit commands are exposed as MCP tools (a curated subset, not the whole CLI) that share the in-process command layer, so both surfaces produce identical results.

Wire the MCP server into every installed agent host in one step (idempotent; reverse with `--uninstall`):

```bash
symora setup mcp                          # auto-detect and wire installed hosts (Claude Code, Codex)
symora setup mcp --dry-run               # show the plan without writing
symora setup mcp --host claude_code      # a specific host only
symora setup mcp --uninstall             # disconnect (removes only the entry it wrote)
```

Or run it directly:

```bash
symora mcp serve                          # stdio (Claude Code, Cursor, etc. use this by default)
symora mcp serve --transport http --port 8765
```

The tool list and input schemas are returned by `tools/list`; mutating tools are marked twice (`Mutates` in the description, `annotations.readOnlyHint: false`) and all support `dry_run`. The server's `initialize` response carries the full usage playbook — tool sequencing, edit addressing, error recovery — so a connected agent needs no further setup.

---

## Repository Links

- [Developer guide](CLAUDE.md)
- [GitHub repository](https://github.com/junyeong-ai/symora)
