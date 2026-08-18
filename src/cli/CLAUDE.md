# src/cli — Command Surface Rules

This layer turns parsed args into JSON output. The contract with downstream agents lives here.

## Output contract

All command output goes through `OutputContext` (`src/cli/output.rs`) and the `Section<T>` / `*Output` types in `src/cli/response/`. Build structured values, hand them to the output context — don't `println!` JSON yourself. Project-relative paths, compact mode, and quiet mode all branch inside the context.

List responses wrap items in `Section<T>`; its fields and stability rules are the single source of truth in `.claude/rules/json-output-stability.md` and the `Section<T>` doc-comment. Reuse `Section<T>` for any new list response rather than hand-rolling one, and shape its `incomplete`/`hints`/`next_commands` through `response::disclosure` (below) rather than setting them by hand.

## Discovery and steering heuristics

Shared discovery and steering heuristics are centralized in `src/cli/symbol_discovery.rs`: broad-query handling, test/noise suppression, ranking hints (`search`, `symbols --name`, and `usage` go through them), `is_single_file_concentration` for the `refs`/`impact` steering gates, and language detection (`resolve_search_languages` / `DetectedLanguages`). It decides what to look for and in what order. If you find yourself reimplementing similar logic in a command handler, move it there instead.

## One vocabulary for what an answer could not do

`src/cli/response/disclosure.rs` owns everything a response says about its own shortfalls, and every search surface routes through it — so a fact stated by one is stated by all, in the same words, with the same remedy.

Two axes, kept apart because their remedies are opposites. `LowerBound` says the answer does not hold everything *its own sources* held (a walk turned away, an index built over unread paths, a page that filled); `CoverageReason`/`Uncovered`/`coverage_shortfall` say a *language* is missing from the answer's domain. Currency — whether a source is up to date — is neither, and lives in `stale`/`backend`/`search index status`.

The shaping is combinators, not per-command assembly: `with_lower_bounds` sets `incomplete`, the prose, and the remedy in one call (a caller that took only the half it needed is how three surfaces came to drop a command the other two emitted), and `with_coverage_disclosure` layers the language gaps and the route's steering on top. Path naming (`relative_paths`, `relative_unread_paths`, `as_paths`, `name_some`) lives here too, so a shortfall reads the same wherever it surfaces. `workspace_route_for` derives *why* an index took no part in an answer from the store outcome — never asserted by a caller, since two surfaces reading the same state must not prescribe opposite remedies.

## Symbol-path resolution

`Symbol::compute_paths_for_all` (in `src/models/symbol/`) is the single source of truth for path strings like `Class/method`. Path matching (exact, `/`-anchored suffix, bare last-component, and `*` wildcard) is what makes `--symbol` flows reliable — keep its semantics stable.

## One anchor for every symbol-level surface

`cli::analysis::resolve_anchor` decides what a `file:line[:column]` addresses, and every symbol-level command — `refs`, `callers`, `callees`, `implementations`, the type hierarchy, `impact`, `context`, `usage`'s location form — routes through it; `edit` shares the same tree rules from `cli::utils::symbol_nav`. A column-less line addresses the symbol declared on it; a line that declares nothing addresses the innermost symbol whose declared range covers it — a body line, or an attribute/doc-comment line the range opens with (so `diff-impact` attributes a hunk on those lines to the item they belong to, as it does a hunk anywhere else in its range). An explicit column is precise, in this order: on a symbol's name (`find_named_at_position`) it is that symbol; otherwise it is the token there — a body token, a parameter, a type or receiver in a signature, an attribute or decorator — resolved through the language server's definition so the analysis anchors at that declaration, wherever it lives (outside the project too; the reference set stays project-local); only a position the server resolves to nothing falls back to the declaration whose header it is on (`column_addressed_symbol`: the keyword or whitespace before a name), and a position that is neither addresses nothing. A usage and its declaration are thus the same question with the same answer, and the position reads exactly as `def`, `hover`, and `rename` read it. `edit`, which addresses declarations in the tree, uses the header rule directly. A definition is mapped into the tree by the name span alone — a file or module definition names the file's first position, a Go receiver a position between a keyword and a method name: spots no name occupies, each a `Binding` there rather than whatever declaration the position falls on. The one exception is a self-definition — the server answering that the queried token IS its own definition, detected from `LocationLink.originSelectionRange` containing the target (rust-analyzer answers a declaration's own `fn`/`async` keyword that way; a plain-`Location` server can only say it by answering the exact queried position): it names no other position, so it reads exactly as no definition at all — the declaration whose header the position sits on, or nothing (a server without name spans keeps a residual: `SymbolInformation` trees, and any server whose `selectionRange` equals its full range, widen the name span to the declaration, so a body position there reads as the enclosing symbol). A definition the symbol tree does not list (a local, a parameter, a module) is a `Binding` anchor: exact, disclosed as such, never dressed up as a symbol. Never resolve a body position to whatever encloses it: that answers a different question than the one asked, and a mutation addressed the same way lands on the wrong symbol. Don't add a second resolver in a command — extend this one and let every surface inherit the change.

## One definition of a symbol's references

`LocationAnalysis` owns what "the references of a symbol" means: project-local, and never the declaration the anchor resolved to. `refs`, `impact`, and `context` project from that one set, so the count they publish under the same name is the same number — from a declaration or from any usage of it. Don't re-filter a raw `find_references` result in a command, and don't add a parameter that lets a call site choose a different meaning for a published field — that is how the three surfaces drifted apart in the first place.

`RefsClassification` then decides what each usage counts as, per POSITION (`services::test_scope`), so a usage inside a `#[cfg(test)]` region of a production file is coverage. File-level classification stays correct for file-shaped facts — `impact`'s per-file rows, `map`'s counts, search ranking.

## One call-graph traversal

Depth-bounded call-graph walks share one core, `src/cli/call_graph.rs` (`walk` over a `Direction` with a `WalkConfig`): `impact`/`blast_radius` walk `Incoming`, `callees --depth`/`--to` walk `Outgoing`. The core owns frontier ordering (sorted → deterministic), the visited set, the depth/fan-out caps, and the lower-bound markers (`max_depth_reached`, truncation). Don't hand-roll a second BFS in a command — extend the core and surface its markers, so a swallowed hop never reads as a genuine empty result.

## One mutation surface

Every command that splices source text by symbol or range lives in `commands/edit.rs`, sharing one root-validated resolution path, one validation gate, one splice core (`LineSplice`), one preview format (an exact hunk derived from the splice — never a re-diff), and one typed output (`EditOutput`). Don't add a second file-writing command; add a subcommand that reduces to the splice core. Previews and safety checks (dangling references on `delete`, optional `--with-diagnostics`) run on the same path for every operation — the destructive path never gets to skip them.

Commands that apply *LSP-computed* edits — `rename`, `actions apply`, `format` — keep their own command files and output types, but must route the actual write through `edit.rs`: `apply_text_edits` (overlap-checked, CRLF- and multibyte-correct) for a single file, `apply_workspace_edits` for a multi-file edit, and `atomic_write` to land bytes. There is no second edit-application implementation, and `atomic_write` is the shared primitive for editing files a user already owns — `setup mcp`'s host-config writes route through it too.

## Fallback messaging on weak LSP servers

Some servers don't implement call hierarchy or `textDocument/typeDefinition`. The right behavior is a structured response that names the missing capability and points to a working alternative — not a silent empty result and not a synthesized one. The `context` command is the canonical example.

## When extending

- Adding a command an agent should drive: also extend the MCP catalog/handler in `src/mcp/tools/`. MCP is a curated subset — navigation, analysis, and edit tools — so not every command becomes a tool; config/doctor/format and machine-setup commands (`commands::setup`, `commands::selfcmd`) stay CLI-only (see `src/mcp/CLAUDE.md`).
- Adding a flag: prefer extending an existing command over forking a new one.
- Removing a command: delete it cleanly. No deprecation shims.
