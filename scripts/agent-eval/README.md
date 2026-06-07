# Agent A/B evaluation harness

Dev/CI tooling that measures whether a headless Claude agent does better
**with** Symora's MCP server than **without** it. It is not shipped code and
is not an MCP tool — it drives the *installed* `symora` binary as a black box,
exactly as a real agent would.

The methodology is pre-registered in
[`docs/benchmarks/methodology.md`](../../docs/benchmarks/methodology.md);
read it first. The short version: identical model + prompt + built-in tools,
the only variable is the MCP config (`--strict-mcp-config`), median of N,
correctness-gated, no tool steering.

## Layout

- `parse-run.mjs` — stream-json → per-run metrics. **Sums tokens per assistant
  turn** (not `result.usage`, which undercounts). `--selftest` verifies this.
- `aggregate.mjs` — per-run metrics → per-arm medians + with/without deltas.
  `--selftest` verifies the math.
- `run-ab.sh` — runs one (repo × task) cell, both arms, N times.
- `tasks/` — pre-registered task prompts (committed before runs).

## Run

```bash
node scripts/agent-eval/parse-run.mjs --selftest    # sanity-check the parser
node scripts/agent-eval/aggregate.mjs --selftest    # sanity-check the math

SYMORA_BIN="$(pwd)/target/release/symora" \
  scripts/agent-eval/run-ab.sh /path/to/repo scripts/agent-eval/tasks/impact.txt 5
```

Requires `claude`, `node`, and a built `symora`. The target repo's language
server must be installed (`symora doctor <lang>`) or the WITH arm's LSP-backed
tools won't resolve — such a cell is recorded skipped, never faked.

## Why a benchmark, and why it isn't checked in with numbers

Symora's structural claims (impact, precise references, safe edit-and-verify)
are only credible once measured against grep+Read on real, post-cutoff repos.
The numbers are produced by running this harness — they are never written by
hand. Lead the matrix with impact/blast-radius, where grep is weakest.
