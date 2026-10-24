# Agent analysis evaluation

This opt-in harness collects headless Claude analysis runs with and without Symora. It does not run in CI or invoke paid agents during ordinary tests. The target must be a clean committed repository with its language server and dependencies ready. The Symora arm exposes the read-only MCP profile, and both arms use plan permissions. A changed target stops the run; use a disposable checkout for evaluation.

```bash
node scripts/agent-eval/parse-run.mjs --selftest
node scripts/agent-eval/aggregate.mjs --selftest
SYMORA_BIN="$(pwd)/target/release/symora" \
  scripts/agent-eval/run-ab.sh /path/to/repo scripts/agent-eval/tasks/impact.txt 5
```

The output directory records the repository revision, binary/agent versions, the model ID and effort level (`EVAL_MODEL`, `EVAL_EFFORT`), task, index build, transcripts, and metrics. Keep it outside the target repository. Freeze a concrete task and ground-truth answer before running; the sample prompt must be adapted to an unambiguous symbol in that revision.

Judge each completed answer against that ground truth and set `correct: true` or `correct: false` in its metrics JSON. Leave the field absent when unjudged. Re-run `node scripts/agent-eval/aggregate.mjs <output-dir> <runs>` after judging. Missing, failed, incorrect, and unjudged runs are counted separately. Only verified correct runs enter the comparison medians; `attempts` retains available metrics from every attempt, so failure cost stays visible. No correctness verdict means no cost-saving comparison.

Report success rate, failure rate, conditional cost/duration medians, and spread together. Token totals are a context-pressure estimate summed per assistant turn, including cached input; use reported dollar cost for financial comparisons. The protocol and publication criteria are in [the evaluation methodology](../../docs/benchmarks/methodology.md).
