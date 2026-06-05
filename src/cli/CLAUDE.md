# src/cli — Command Surface Rules

This layer turns parsed args into JSON output. The contract with downstream agents lives here.

## Output contract

All command output goes through `OutputContext` (`src/cli/output.rs`) and the `Section<T>` / `*Output` types in `src/cli/response/`. Build structured values, hand them to the output context — don't `println!` JSON yourself. Project-relative paths, compact mode, and quiet mode all branch inside the context.

List responses wrap items in `Section<T>`; its fields and stability rules are the single source of truth in `.claude/rules/json-output-stability.md` and the `Section<T>` doc-comment. Reuse `Section<T>` for any new list response rather than hand-rolling one.

## Discovery heuristics

Broad-query handling, test/noise suppression, and ranking hints are centralized in `src/cli/symbol_discovery.rs`. `search`, `symbols --name`, and `usage` all go through it. If you find yourself reimplementing similar logic in a command handler, move it there instead.

## Symbol-path resolution

`Symbol::compute_paths_for_all` (in `src/models/symbol/`) is the single source of truth for path strings like `Class/method`. Path matching (substring, suffix, exact) is what makes `--symbol` flows reliable — keep its semantics stable.

## Fallback messaging on weak LSP servers

Some servers don't implement call hierarchy or `textDocument/typeDefinition`. The right behavior is a structured response that names the missing capability and points to a working alternative — not a silent empty result and not a synthesized one. The `context` command is the canonical example.

## When extending

- Adding a command: also extend the MCP catalog/handler in `src/mcp/tools/` so both surfaces stay in lockstep. Exception: lifecycle commands under `commands::setup` and `commands::selfcmd` are CLI-only by design (see `src/mcp/CLAUDE.md`).
- Adding a flag: prefer extending an existing command over forking a new one.
- Removing a command: delete it cleanly. No deprecation shims.
