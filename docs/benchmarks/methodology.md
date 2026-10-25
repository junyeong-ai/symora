# Agent analysis evaluation protocol

Measure whether Symora helps an agent answer structural code questions correctly at useful cost and latency. A cheaper incorrect answer is a failure. No performance claim follows from the existence of the harness or its unit tests.

The harness and how to run it are in [`scripts/agent-eval/`](../../scripts/agent-eval/README.md).

## Freeze a cell before running

Record a repository revision, a concrete task, its ground truth, the model ID, the effort level, the tool versions, and the target's dependency/language-server environment. Effort levels do not mean the same amount of reasoning across models, so a comparison holds the level fixed and states it. Choose tasks that exercise different capabilities: exact references with shadowed names, caller reach, a package boundary, and a documentation/configuration control. Include a dynamic or structurally typed case and a serverless case when evaluating general applicability. The sample task is a template, not a frozen ground-truth question.

Use a disposable, clean checkout. Both arms keep the same task, model, built-in tools, and source tree; only the read-only MCP configuration differs. Do not instruct the agent to prefer Symora. Record whether it actually used the tools. Measure startup/index construction separately from repeated query cost. Balance arm order when comparing latency or dollars; the convenience runner's sequential arms alone do not establish a causal performance result.

## Judge correctness independently of cost

Create the expected file/site set or explanation rubric before seeing the answers. A reviewer checks each completed answer without seeing its cost or arm where practical. Record an explicit boolean `correct` in each metrics file. Unjudged answers do not enter comparison medians. A task needing a different answer after a source change is a new cell.

Report attempted, correct, incorrect, failed, and unjudged counts. Missing output is a failed attempt, not an omitted observation. Publish success/failure rates alongside medians and spread for correct answers, and include the available cost of all attempts. Never convert unknown costs into zeroes or exclude failure rates from a claimed improvement.

## Cost and maintenance

The parser's token sum includes cache reads and represents cumulative context pressure, not unique tokens or dollars. Use the agent's reported dollar cost and duration for those comparisons. Small deterministic regression tests cover parser/accounting errors. Run paid agent comparisons when changing tool selection, output design, or retrieval behavior warrants them, rather than making them a gate on every code change.

Publish results only with pinned cells, ground truth, verdicts, counts, versions, and startup-state information. The convenience runner measures the read-only MCP profile; it does not exercise the CLI skill, so a claim about the skill needs an arm of its own. Repositories and tasks should span the intended users; a Rust-only result cannot establish Python Protocol handling, multilingual retrieval, or remote MCP behavior.
