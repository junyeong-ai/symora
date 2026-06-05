# src/mcp — MCP Server Rules

`symora mcp serve` exposes a Model Context Protocol surface (stdio + HTTP) over the same in-process command layer the CLI uses. Each tool maps 1:1 to a CLI command shape.

## Catalog and handlers must stay in lockstep

Three things are co-versioned for every tool:

1. An entry in `tools/catalog.rs` (the `tools/list` schema)
2. An input struct in `tools/handlers.rs`
3. A branch in the `dispatch` match

A test in `tools/mod.rs` enumerates the required tool names. Adding a tool means touching all three; removing one means removing all three. The test will fail if they drift.

## Mutating tools advertise themselves

Any tool that writes source files must contain the literal word `Mutates` in its description. A second test enforces this so a reviewer can spot mutation from the catalog without reading handler code.

## Location input convention

Tools that accept a `file:line:column` target embed `LocationInput` via `#[serde(flatten)]` rather than redeclaring the three fields. New location-taking tools follow the same pattern.

## Output discipline

Handlers run the underlying command against a `BufferedSink` and return whatever JSON the command emitted. Don't post-process at the handler — the CLI's output contract is the MCP contract. If a tool needs a different output shape, add it to the CLI command first. The server layer re-emits that same JSON as `structuredContent` alongside the text block and sets `isError` from the captured error flag; that envelope is the only shaping, and it stays a faithful pass-through.

## Lifecycle commands are CLI-only

`setup`, `setup skill`, `setup deps`, `self update`, `self uninstall` are deliberately not exposed as MCP tools. They mutate the user's machine outside the project boundary — installing skills under `~/.claude`, running package managers, replacing the running binary, removing config — and an AI agent should never drive those flows. The 1:1 CLI↔MCP mapping rule above does not apply to commands under `cli::commands::setup` or `cli::commands::selfcmd`. Keep them out of the catalog.
