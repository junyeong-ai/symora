# src/mcp — MCP Server Rules

`symora mcp serve` exposes a Model Context Protocol surface (stdio + HTTP) over the same in-process command layer the CLI uses. The catalog is a curated subset of the CLI — navigation, analysis, and edit tools — and each tool is backed by one CLI command, mirroring its input (a tool may omit or pin an option an agent shouldn't set, e.g. `get_context` fixes `body: false`).

## Catalog and handlers must stay in lockstep

Four things are co-versioned for every tool:

1. An entry in `tools/catalog.rs` (the `tools/list` schema)
2. An input struct in `tools/handlers.rs`
3. A branch in the `dispatch` match
4. Every field the input struct deserializes must be an advertised property in the catalog schema. `dispatch` rejects unknown argument keys against the catalog (`check_unknown_arguments`), so an undeclared-but-deserialized field is unreachable at runtime — adding a tool option means touching the catalog entry and the input struct in the same change.

Tests in `tools/mod.rs` enumerate the required tool names and assert every handler input field is an advertised property. Adding a tool means touching all four; removing one means removing all four. The tests will fail if they drift. Property names mirror the backing command's field (`path` on get_file_overview vs `file` on list_file_symbols is intentional, not drift).

## Mutating tools advertise themselves — twice, in lockstep

Mutation is one fact stated two ways: the typed `annotations.read_only_hint` (constructed via `ToolDefinition::read_only` / `ToolDefinition::mutating` — there is no neutral constructor, so the decision can't be skipped) and the literal word `Mutates` in the description for human reviewers. A biconditional test over the whole catalog enforces that they agree in both directions. The read-only profile (`mcp serve --profile read-only`) filters on the typed hint, never on description text, and gates `tools/call` as well as `tools/list` — a hidden tool that still dispatched would make the boundary cosmetic.

## The instructions playbook stays honest by test

`initialize` returns a usage playbook (`instructions.rs`) that hosts inject into the model's context. Backticks in that text are reserved for tool names; a test asserts every backtick-quoted token exists in the catalog, so renaming or removing a tool cannot leave the playbook stale. Mention new tools there when they change how an agent should sequence calls.

## Location input convention

Tools that accept a `file:line:column` target embed `LocationInput` via `#[serde(flatten)]` rather than redeclaring the three fields. New location-taking tools follow the same pattern. The edit tools embed `EditTargetInput` instead (file plus exactly one of symbol or line); navigation/analysis tools keep `LocationInput`.

## Output discipline

Handlers run the underlying command against a `BufferedSink` and return whatever JSON the command emitted. Don't post-process at the handler — the CLI's output contract is the MCP contract. If a tool needs a different output shape, add it to the CLI command first. The server layer re-emits that same JSON as `structuredContent` alongside the text block and sets `isError` from the captured error flag; that envelope is the only shaping, and it stays a faithful pass-through.

## Lifecycle commands are CLI-only

`setup`, `setup skill`, `setup deps`, `setup mcp`, `self update`, `self uninstall` are deliberately not exposed as MCP tools. They mutate the user's machine outside the project boundary — installing skills under `~/.claude`, writing MCP entries into agent-host configs, running package managers, replacing the running binary, removing config — and an AI agent should never drive those flows. The 1:1 CLI↔MCP mapping rule above does not apply to commands under `cli::commands::setup` or `cli::commands::selfcmd`. Keep them out of the catalog.
