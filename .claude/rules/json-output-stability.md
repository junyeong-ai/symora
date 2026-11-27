---
paths:
  - "src/cli/response/**/*.rs"
  - "src/cli/output*.rs"
  - "src/cli/errors.rs"
  - "src/cli/blast_radius.rs"
  - "src/cli/file_symbols.rs"
  - "src/cli/analysis.rs"
  - "src/cli/call_graph.rs"
  - "src/cli/symbol_discovery.rs"
  - "src/models/lsp.rs"
  - "src/cli/commands/**/*.rs"
  - "src/daemon/wire*.rs"
  - "src/daemon/protocol.rs"
  - "src/mcp/tools/**/*.rs"
---

# JSON output contract

CLI JSON is the public API. MCP tool results pass the same command output through as text and `structuredContent`; handlers do not reshape it. Daemon wire types preserve the model's meaning. Change these surfaces together, and treat field removal or renaming as a breaking change.

## Lists and budgets

Use `Section<T>` for enumerated sets whose length grows with the repository or finding count:

- `count`: total matches found in the consulted sources; a lower bound when qualified by `incomplete` or a coverage gap.
- `showing`: number emitted in `items`.
- `items`: whole result objects.
- `truncated`: present and true when `showing < count`.

In every object, the fields that qualify a list (`config_errors`, `error`, `truncated`, `stale`, `incomplete`, `indexing`, `coverage_gaps`, `unread_paths`, `unmeasured_files`, `bodies_included`, `body_omitted`, `*_status`, `*_status_counts`, `hints`, `next_commands`) precede the object's lists, so a reader that sees only the head of a response still has them. Declare output fields in that order; the output layer also emits each section's `items` last and leads with injected `config_errors`. `tests/common/mod.rs` checks every response the integration tests parse.

Bounded summaries and rankings without an enumeration claim may remain arrays. Map's individually limited lists use their own sections; a neighbouring subtree total does not count an immediate directory listing.

A command-level token budget applies inside the command; `pack` stays within its character-based estimate, which is not a model-specific token count or an envelope limit. Other responses use `output.max_response_chars` (default 20000; 0 disables) as a sizing target. The output layer measures compact JSON independently of display format. Source-bearing commands identify body paths explicitly through `print_source`; whole bodies are omitted before list items, preserving declaration identities and disclosing `body_omitted: response_size`. Never discover source bodies by matching arbitrary JSON keys. List fitting preserves totals and adds a size-limit hint. Fixed metadata may still exceed the target. Never slice strings or return invalid JSON to meet it. Recount `bodies_included` after fitting; omit it when its section becomes empty.

## Provenance and limits

Keep these meanings distinct:

| Field | Meaning |
|---|---|
| `backend` | Producer: `index`, `workspace`, `document`, `scan`, or `ast`, according to the surface |
| `stale` | Consulted index rows came from files changed since indexing; retained conservatively after partial response-size fitting |
| `incomplete` | The answer did not enumerate everything its own sources held |
| `coverage_gaps` | Named languages could not be covered, with a machine-readable reason |
| `indexing` | An LSP-dependent query ran under degraded workspace analysis |
| `degraded_column` | A read result's column could not be decoded against readable source |
| `unread_paths` | Project-relative paths the walk or build could not read |

Index hits are snapshot answers. Rows cannot reveal a newly added file; the build records a fingerprint per language, and where one no longer agrees with that language's files — or cannot be read at all — disclose it separately in hints for the answered languages it concerns and prescribe the appropriate refresh. Apply this to positive and empty results. Currency checks compare the file-size policy and discovered paths, sizes, and modification times; metadata-preserving changes require rebuilding and cannot be certified by this check. An unqualified zero is only an absence within the command's documented project, language, ignore, and size scope. Never turn it into a claim about runtime reachability.

`incomplete` is set for unread paths, partially read index builds, unmaterialised overlap between sources, source-side candidate caps, a workspace-wide server answer confined to requested paths afterwards, a name no server search can be asked for (a computed member with nothing enclosing it), and a call-graph walk's depth cap, per-node fan-out cap or failed hop, each still named by its own flag (`max_depth_reached`, `callers_truncated`/`callees_truncated`, `failed_hops`). It is the one bit that says a count is a lower bound because of the command's own caps or failures; a reader absent it may take the count as everything those sources held. Display truncation alone does not prove the total is a lower bound: `truncated` means fewer were shown than counted, and the count stays exact. A language gap alone does not set `incomplete` as well. Documented exclusions (hidden paths, binary files, configured size limits) are outside the domain, rather than read failures.

A `coverage_gaps` item has `language` and `reason`; `unavailable` also carries `message`. Reasons are `not_indexed`, `server_not_installed`, `not_consulted`, `timed_out`, `unsupported`, `unavailable`, and `not_searched`. Order gaps by language and lead remedies with failures that actually ran. Do not prescribe a rebuild for a language the index cannot extract.

Use `response::disclosure` to assemble facts and remedies. A failed read names its paths; a running build prescribes waiting, not starting another; an unavailable source names its actual failure. Keep each fact with its remedy when advice is capped. Do not infer current permissions or currency from an earlier failure.

## Positions

An item states its own position flat: `file`, `line`, `column`, where the column is a declaration's name start, the position the next query takes. Omit `file` only where the parent names it (diagnostics, pack). A span adds `end_line` (inclusive) and `end_column` (exclusive), each omitted when the source does not state it, never filled in. A second position related to the item (`call_site`, `definition`, `target`) is a nested object of the same flat shape. A symbol is identified by `name`, `kind`, and `name_path` where there is one, beside that position. `map` and `pack` give lines without columns. `LocationOutput` is the shared shape: embed it with `#[serde(flatten)]` for the item's own position and nest it for a related one.

## Symbols and analysis

File-scoped symbols carry a response-level `backend`: `document` is the language server's nested tree; `ast` is flat with containment in `name_path`. In a code language, a value the nested tree binds directly in a function, method or constructor, a parameter or local, is not listed: as the grammar reads it, it is no declaration. A `--symbol` path the server's tree holds nothing on is answered by the grammar's reading of the file, as `ast`, where that reading holds it. A `document` row's `name_path` appends `[kind]` where siblings share a name; an AST row's does not, so two AST rows can share one. Workspace resolution carries provenance per row. A map overview omits `focus_symbols` and `backend` together when no producer could read its declarations.

`score` is ranking relevance, not a probability. `is_exported` describes the target declaration's own export/public visibility, not accessibility through enclosing types, modules, re-exports, or build configuration. Read it from the exact AST declaration; omit it when syntax or context does not determine the answer. Call-graph output reports observed counts, depth/fan-out limits, indexing degradation, failed hops, and implementation-lookup limits. It does not publish a calibrated risk or confidence verdict. Test-reference counts and `test_caller_ratio` describe static references/callers, not execution coverage or change safety.

For JavaScript/TypeScript, an AST export wrapper establishes `true`; without one, only declaration forms that cannot themselves be exported establish `false`. Other declarations omit the field rather than treating the absence of a parsed wrapper as proof.

Reference counts on `symbols --refs` rows and `diff-impact`'s added and modified rows (`refs`, `test_refs`, `prod_refs`) carry `refs_status`: `indexing_degraded` when the query ran under degraded analysis, so they are lower bounds, and otherwise why no count was taken, in the `coverage_gaps` reasons (`timed_out`, `unsupported`, `server_not_installed`, `not_consulted`, `unavailable` for a failed query or one not asked), with the counts absent rather than zero. A `diff-impact` row's `callers` is a `Section` cut at `lsp.calls_limit`, absent where no list was obtained. With `--callers` it is read off the references the row's count was taken from, `callers_status: references_derived`: each function, method or constructor whose code holds one, once, at the first it holds, so a use as a value there names a caller as a call does, and a reference outside every callable (an import, a module-level annotation) names none. A row that is no function, method or constructor reads `not_callable` and has none, since its references are uses rather than calls. With `--call-hierarchy` it is the server's incoming calls, its `callers_status` absent from an authoritative list. Either way `callers_status` is otherwise `indexing_degraded` or one of the `refs_status` failure reasons, and `unavailable` where no count was taken to derive or ask callers from, or a referencing file's declarations could not be read. `callers` with no call hierarchy reads references the same way, also marked `references_derived`. A deletion whose declarations were not identified carries no `name` or `kind`, only its position. A row marked `timed_out` adds a hint naming `lsp.timeout_secs`. A row the grammar answered is asked about where the language server reads the same declaration — its symbol that names it, or any definition it names at the row's position — and not at all where it names only other declarations there, or none; `edit delete` reports that reference check as `references_status: unavailable`, as it does a failed one. `diff-impact`'s `total_references` sums the measured rows and is absent when none was measured; beside it, `refs_status_counts` counts the rows of each `refs_status` and `callers_status_counts` those of each `callers_status`, `{status: rows}`, each absent when no row carries one, so one row a server could not answer is counted as one row rather than standing for the diff.

Location analysis echoes the resolved `target` by name, kind, and its own `file`, `line`, `column`. A declaration the document-symbol tree omits — a local, a parameter, or one the server's outline leaves out — resolves as a `binding`, named by the text of the reference the server reports at its declaration. `not_a_symbol` and `unavailable` are not resolved declarations; their `target`, and a binding's without such a reference, is named `symbol@line:column`. A position a server defines nothing at reads `not_a_symbol`, except where the server's readiness wait ran out (`indexing: timed_out`), when it reads `unavailable`. A member's zero references may still permit construction, dispatch, or structural conformance through its declaring type; a free function can also be reached through registration or reflection. Never label either unused from a static zero alone.

`declared_symbols` on a packed file counts its AST-derived signatures and is present when the per-file cap or token budget omitted some. `context` shares `body_tokens` across target, callees, and types in that order. `body_omitted` names `token_budget` or `response_size` when that budget omitted a requested body. `bodies_included` counts complete attached bodies; an omitted body can mean exhausted budget, an unresolvable location, or a declaration without a body.

`diff-impact` discloses unmeasured files, candidate caps and, measuring the working tree, the untracked files no diff holds. Documentation and configuration files (Markdown, MDX, YAML, TOML, JSON) are unmeasured files with a hint naming them, never a row per key or heading their servers list. `--max-symbols` bounds the added and modified rows, whose references are asked, and apart from them the deleted rows, so a deletion never takes a counted row's place; the analysis stops at the first file the cap leaves no room for a counted row in, and `incomplete` says either bound left rows out. Deletions read declarations from the base revision; a removal whose declarations are not identified is placed at the first line it removed from the base's version of the file. Changed body lines belong to their enclosing declaration. A staged analysis cannot use working-tree symbols over unstaged line changes as though they described the git staging area.

## Index state

Read rows and their completed-build scope in the same database transaction. Per-file refreshes do not establish whole-project coverage. `search index status` always carries `languages`; empty means no completed symbol build. `last_indexed` dates that completion, not the newest row. Persist `unread_paths` with completion and retire a path when a later refresh settles it.

`index_size_bytes` is the database's page count times page size, including committed WAL pages. It belongs to status, not an individual build. A conflicting build or lock is transient contention; retry after it releases.

## Errors and optional fields

Failures use `{ "error": { "code", "message", "hint" } }` on stdout and a nonzero CLI exit. `hint` is optional. Omit unavailable optional fields instead of filling them with empty strings or fabricated zeroes.

`conflict` means a revision changed or a resource is held. `precondition_failed` means an asserted condition is unmet or unverified. A multi-file edit can fail after earlier files were committed; its error names those files and must not invite an unexamined whole-operation retry.

`config_errors` is a top-level array explaining ignored or invalid configuration. The output layer adds it without overwriting a command's more specific diagnostics. A result produced under defaults must not imply the intended configuration applied.

Doctor's `installed` means an executable resolves; `serves` means a fresh server answers the handshake. Python rows carry `python_interpreter` with source `settings`, `project_environment`, or `server_default`; `python_path` is present only when the client sends a selection. These facts do not verify import resolution or reference completeness. Configured server overrides carry `source: config` and `command`. A warm daemon retains its project configuration until project restart (`daemon restart --project`), daemon restart, or idle eviction, so a configuration edit does not by itself replace a running server. Project restart preserves other sessions and rejects active project requests or pending server startup with `conflict`.

`indexing` values are `timed_out` (the readiness wait expired) and `unconfirmed` (workspace readiness was not confirmed throughout the query). A busy/ready cycle during the query remains unconfirmed; a late ready signal cannot certify an earlier answer. Timeouts belong to individual waits, not persistent server state. Log text, document diagnostics, project health, and drained progress tasks cannot confirm workspace readiness. A server that reports no workspace status stays unconfirmed; do not promise that retrying will establish completeness. Rename applies only when its edit set carries no indexing degradation, or when the caller accepts an `unconfirmed` one with `--accept-unconfirmed`, which the response keeps marked; `timed_out` allows previews only.
