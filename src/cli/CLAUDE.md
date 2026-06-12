# src/cli — Command Surface Rules

This layer turns parsed args into JSON output. The contract with downstream agents lives here.

## Output contract

All command output goes through `OutputContext` (`src/cli/output.rs`) and the `Section<T>` / `*Output` types in `src/cli/response/`. Build structured values, hand them to the output context — don't `println!` JSON yourself. Project-relative paths, compact mode, and quiet mode all branch inside the context.

List responses wrap items in `Section<T>`; its fields and stability rules are the single source of truth in `.claude/rules/json-output-stability.md` and the `Section<T>` doc-comment. Reuse `Section<T>` for any new list response rather than hand-rolling one.

## Discovery and steering heuristics

Shared discovery and steering heuristics are centralized in `src/cli/symbol_discovery.rs`: broad-query handling, test/noise suppression, and ranking hints (`search`, `symbols --name`, and `usage` go through them), plus the gating predicates and disclosure vocabulary behind `hints`/`next_commands` — `is_single_file_concentration` for the `refs`/`impact` steering gates and `coverage_reason` for the `search`/`usage` coverage disclosures. If you find yourself reimplementing similar logic in a command handler, move it there instead.

## Symbol-path resolution

`Symbol::compute_paths_for_all` (in `src/models/symbol/`) is the single source of truth for path strings like `Class/method`. Path matching (substring, suffix, exact) is what makes `--symbol` flows reliable — keep its semantics stable.

## One mutation surface

Every command that splices source text by symbol or range lives in `commands/edit.rs`, sharing one root-validated resolution path, one validation gate, one splice core (`LineSplice`), one preview format (an exact hunk derived from the splice — never a re-diff), and one typed output (`EditOutput`). Don't add a second file-writing command; add a subcommand that reduces to the splice core. Previews and safety checks (dangling references on `delete`, optional `--with-diagnostics`) run on the same path for every operation — the destructive path never gets to skip them.

Commands that apply *LSP-computed* edits — `rename`, `actions apply`, `format` — keep their own command files and output types, but must route the actual write through `edit.rs`: `apply_text_edits` (overlap-checked, CRLF- and multibyte-correct) for a single file, `apply_workspace_edits` for a multi-file edit, and `atomic_write` to land bytes. There is no second edit-application implementation, and `atomic_write` is the shared primitive for editing files a user already owns — `setup mcp`'s host-config writes route through it too.

## Fallback messaging on weak LSP servers

Some servers don't implement call hierarchy or `textDocument/typeDefinition`. The right behavior is a structured response that names the missing capability and points to a working alternative — not a silent empty result and not a synthesized one. The `context` command is the canonical example.

## When extending

- Adding a command an agent should drive: also extend the MCP catalog/handler in `src/mcp/tools/`. MCP is a curated subset — navigation, analysis, and edit tools — so not every command becomes a tool; config/doctor/format and machine-setup commands (`commands::setup`, `commands::selfcmd`) stay CLI-only (see `src/mcp/CLAUDE.md`).
- Adding a flag: prefer extending an existing command over forking a new one.
- Removing a command: delete it cleanly. No deprecation shims.
