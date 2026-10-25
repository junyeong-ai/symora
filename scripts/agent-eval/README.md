# Agent analysis evaluation

This opt-in harness collects headless Claude analysis runs with and without Symora. It does not run in CI or invoke paid agents during ordinary tests. The target must be a clean committed repository with its language server and dependencies ready. The Symora arm exposes the read-only MCP profile, and both arms use plan permissions. A changed target stops the run; use a disposable checkout for evaluation.

```bash
node scripts/agent-eval/parse-run.mjs --selftest
node scripts/agent-eval/aggregate.mjs --selftest
SYMORA_BIN="$(pwd)/target/release/symora" \
  scripts/agent-eval/run-ab.sh /path/to/repo scripts/agent-eval/tasks/impact.txt 5
```

The output directory records the repository revision, binary/agent versions, the model ID and effort level (`EVAL_MODEL`, `EVAL_EFFORT`), task, index build and its wall-clock seconds, transcripts, and metrics. Keep it outside the target repository. Freeze a concrete task and ground-truth answer before running; the sample prompt must be adapted to an unambiguous symbol in that revision.

Judge each completed answer against that ground truth and set `correct: true` or `correct: false` in its metrics JSON, leaving it absent when unjudged, then re-run `node scripts/agent-eval/aggregate.mjs <output-dir> <runs>`; its `attempts` keep the metrics available from every attempt, failed ones included. How runs are counted, compared and reported is in [the evaluation methodology](../../docs/benchmarks/methodology.md).
