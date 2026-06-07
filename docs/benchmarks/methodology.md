# Symora A/B Benchmark — Pre-Registration

This document is **frozen before any run**. It fixes what is measured, on
which repos, and how an answer is judged correct, so the published matrix
cannot be a post-hoc selection of flattering cells. Changing it after seeing
results is a protocol violation — add a dated amendment instead.

## Hypothesis

An agent equipped with Symora's MCP tools resolves structural questions
(impact / references / safe edits) with **fewer Read/Grep calls and fewer
tokens, at equal-or-better correctness**, than the same agent with grep+Read
only. The edge **grows with repo size and task complexity** and **does not
regress** on tasks where Symora offers no structural advantage.

## Design

- **Two arms, one variable.** Identical model (`opus`), identical prompt,
  identical built-in tools (Read/Grep/Glob/Bash stay available in both).
  The only difference is the MCP config: WITH = `symora mcp serve`,
  WITHOUT = empty, both pinned with `--strict-mcp-config` so no ambient MCP
  leaks in.
- **No tool steering.** Prompts never mention Symora. Tool choice is
  low-salience; injecting "prefer symora" would measure the wording, not the
  tool. Whether the agent reaches for symora unprompted is itself reported
  (`symora_calls`).
- **Median of N = 5.** Run-to-run variance is large; headlines are medians,
  spread (min/max) is shown, and no claim rests on a single run.

## Repos (pinned at commit SHA before runs)

Chosen for **strong Symora LSP support** and to span size tiers, with a
**control** where no structural advantage is expected (no regression must
show). Prefer **post-cutoff commits** of actively-developed projects to
resist memorized answers.

| Tier | Repo | SHA | Language |
|------|------|-----|----------|
| small | _TBD before run_ | _pin_ | Rust |
| medium | _TBD before run_ | _pin_ | Rust |
| control | _TBD before run_ | _pin_ | (docs/config task) |

## Tasks (frozen)

Lead with where grep is demonstrably weakest and Symora is differentiated —
**not** plain text search. One task per category per repo, phrased as a real
user would, identical across arms:

1. **Impact / blast-radius** — "If I change the signature of `X`, what
   breaks?" (Symora: `get_impact`; grep finds only string matches.)
2. **Precise references / callers** — "Who calls `X`?" where the name is
   shadowed or overloaded (Symora disambiguates; grep cannot).
3. **Flow / navigation** (breadth/fairness) — "How does `X` reach `Y`?"
4. **Control** — a question with no structural angle (config/docs), to prove
   no regression.

Task prompts live in `scripts/agent-eval/tasks/` and are committed in the
same pre-registration commit as this file.

## Metrics (from `parse-run.mjs`)

- **tokens** — context tokens (`input + output + cache_creation +
  cache_read`), **summed per assistant turn**. Never `result.usage`, which is
  last-turn only and undercounts. This is a context-pressure proxy, **not
  dollars** (cache_read re-counts the conversation each turn); dollars come
  from `cost_usd`. (Verified by the parser self-test.)
- **read / grep / read_grep_total** — the fall-back-to-grep signal Symora
  aims to displace. Bash invocations that shell out to grep/rg/find count as
  grep.
- **symora_calls** — distinguishes "symora sufficed" from "agent ignored it."
- **turns** — count of assistant turns (`result.num_turns` is also captured).
  **duration_ms, cost_usd** — cumulative, taken from the `result` event.
- A run with no terminating `result` event (crash / budget cap / never
  started) is flagged `failed` and **excluded from the medians**, never
  counted as a legitimate zero.

## Correctness rubric (gates every cell)

A cheaper-but-wrong answer is a **loss**, not a win. Each task has a
ground-truth key authored from the repo before runs:

- **Deterministic** where possible — the answer must name the expected file
  set; an edit task must leave the repo compiling (build a scratch copy).
- **Blind LLM judge** for free-form flow answers, scored against the key,
  not told which arm produced the answer.

## Validity guards

- A WITH-arm run whose `system/init` event does not list `mcp__symora__*`
  tools (parsed into `symora_tools_exposed` — e.g. wrong binary path, bad
  MCP config) is marked `failed` by the harness and **excluded from the
  medians**: it would measure "no MCP", not Symora.
- Report ties and losses honestly; short single-flow tasks can show flat/
  higher cost on the WITH arm (tool definitions sit in context and short
  tasks don't amortize them) — that is the expected "value scales with
  complexity" boundary, not a result to hide.
- The target repo's language server is an operator precondition
  (`symora doctor <lang>` before running); a cell run without it is not
  publishable.

## Output

Results land in `docs/benchmarks/ab-matrix.md` (one row per cell: WITH vs
WITHOUT medians + delta %, N, SHA, date), committed **after** runs. This
methodology commit lands **before** them.
