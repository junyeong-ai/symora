---
paths:
  - "src/cli/response/**/*.rs"
  - "src/cli/output*.rs"
  - "src/cli/errors.rs"
  - "src/cli/blast_radius.rs"
  - "src/cli/file_symbols.rs"
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

Every field that qualifies a list precedes it: the output layer emits a section's `items` last, a response's own bulk list (`diff-impact` `changes`, `impact` `files`) is its last field, and injected `config_errors` leads the response, so a reader that sees only the head still has the counts and markers.

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

Index hits are snapshot answers. Rows cannot reveal a newly added file; when the build's tree fingerprint no longer agrees with the tree, disclose that separately in hints and prescribe the appropriate refresh. Apply this to positive and empty results. Currency checks compare the file-size policy and discovered paths, sizes, and modification times; metadata-preserving changes require rebuilding and cannot be certified by this check. An unqualified zero is only an absence within the command's documented project, language, ignore, and size scope. Never turn it into a claim about runtime reachability.

`incomplete` is set for unread paths, partially read index builds, unmaterialised overlap between sources, and source-side candidate caps. Display truncation alone does not prove the total is a lower bound. A language gap alone does not set `incomplete` as well. Documented exclusions (hidden paths, binary files, configured size limits) are outside the domain, rather than read failures.

A `coverage_gaps` item has `language` and `reason`; `unavailable` also carries `message`. Reasons are `not_indexed`, `server_not_installed`, `not_consulted`, `timed_out`, `unsupported`, `unavailable`, and `not_searched`. Order gaps by language and lead remedies with failures that actually ran. Do not prescribe a rebuild for a language the index cannot extract.

Use `response::disclosure` to assemble facts and remedies. A failed read names its paths; a running build prescribes waiting, not starting another; an unavailable source names its actual failure. Keep each fact with its remedy when advice is capped. Do not infer current permissions or currency from an earlier failure.

## Symbols and analysis

File-scoped symbols carry a response-level `backend`: `document` is the language server's nested tree; `ast` is flat with containment in `name_path`. Workspace resolution carries provenance per row. A map overview omits `focus_symbols` and `backend` together when no producer could read its declarations.

`score` is ranking relevance, not a probability. `is_exported` describes the target declaration's own export/public visibility, not accessibility through enclosing types, modules, re-exports, or build configuration. Read it from the exact AST declaration; omit it when syntax or context does not determine the answer. Call-graph output reports observed counts, depth/fan-out limits, indexing degradation, failed hops, and implementation-lookup limits. It does not publish a calibrated risk or confidence verdict. Test-reference counts and `test_caller_ratio` describe static references/callers, not execution coverage or change safety.

For JavaScript/TypeScript, an AST export wrapper establishes `true`; without one, only declaration forms that cannot themselves be exported establish `false`. Other declarations omit the field rather than treating the absence of a parsed wrapper as proof.

Navigation location objects carry file, line, and column. Location analysis echoes a summary `target` with file and line, without a column. A local or parameter may be a `binding` even when the document-symbol tree omits it. `not_a_symbol` and `unavailable` are not resolved declarations. A member's zero references may still permit construction, dispatch, or structural conformance through its declaring type; a free function can also be reached through registration or reflection. Never label either unused from a static zero alone.

`declared_symbols` on a packed file counts its AST-derived signatures and is present when the per-file cap or token budget omitted some. `context` shares `body_tokens` across target, callees, and types in that order. `body_omitted` names `token_budget` or `response_size` when that budget omitted a requested body. `bodies_included` counts complete attached bodies; an omitted body can mean exhausted budget, an unresolvable location, or a declaration without a body.

`diff-impact` discloses unmeasured files and candidate caps. Deletions read declarations from the base revision; changed body lines belong to their enclosing declaration. A staged analysis cannot use working-tree symbols over unstaged line changes as though they described the index.

## Index state

Read rows and their completed-build scope in the same database transaction. Per-file refreshes do not establish whole-project coverage. `search index status` always carries `languages`; empty means no completed symbol build. `last_indexed` dates that completion, not the newest row. Persist `unread_paths` with completion and retire a path when a later refresh settles it.

`index_size_bytes` is the database's page count times page size, including committed WAL pages. It belongs to status, not an individual build. A conflicting build or lock is transient contention; retry after it releases.

## Errors and optional fields

Failures use `{ "error": { "code", "message", "hint" } }` on stdout and a nonzero CLI exit. `hint` is optional. Omit unavailable optional fields instead of filling them with empty strings or fabricated zeroes.

`conflict` means a revision changed or a resource is held. `precondition_failed` means an asserted condition is unmet or unverified. A multi-file edit can fail after earlier files were committed; its error names those files and must not invite an unexamined whole-operation retry.

`config_errors` is a top-level array explaining ignored or invalid configuration. The output layer adds it without overwriting a command's more specific diagnostics. A result produced under defaults must not imply the intended configuration applied.

Doctor's `installed` means an executable resolves; `serves` means a fresh server answers the handshake. Python rows carry `python_interpreter` with source `settings`, `project_environment`, or `server_default`; `python_path` is present only when the client sends a selection. These facts do not verify import resolution or reference completeness. Configured server overrides carry `source: config` and `command`. A warm daemon retains its project configuration until project restart (`daemon restart --project`), daemon restart, or idle eviction, so a configuration edit does not by itself replace a running server. Project restart preserves other sessions and rejects active project requests or pending server startup with `conflict`.

`indexing` values are `timed_out` (the readiness wait expired) and `unconfirmed` (workspace readiness was not confirmed throughout the query). A busy/ready cycle during the query remains unconfirmed; a late ready signal cannot certify an earlier answer. Timeouts belong to individual waits, not persistent server state. Log text, document diagnostics, project health, and drained progress tasks cannot confirm workspace readiness. Servers without a supported completion channel may remain unconfirmed; do not promise that retrying will establish completeness. Rename applies only when its edit set carries no indexing degradation; `unconfirmed` and `timed_out` allow previews only.
