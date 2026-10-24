#!/usr/bin/env bash
# A/B benchmark for one (repo × task) cell: run a headless Claude agent with
# and without Symora's MCP server, N times each, and emit per-run metrics.
#
# The ONLY variable between arms is the MCP config — identical model, prompt,
# and built-in tools — so the measurement isolates Symora's effect, not prompt
# engineering. The prompt must NOT mention Symora (steering is low-salience and
# would measure the wording, not the tool); whether the agent reaches for
# symora unprompted is itself a reported metric.
#
# Usage:
#   SYMORA_BIN=/abs/path/to/symora \
#   scripts/agent-eval/run-ab.sh <repo-dir> <task-file> [runs] [out-dir]
#
# The model and effort level are pinned and recorded with the cell:
# EVAL_MODEL (a model ID, not an alias) and EVAL_EFFORT override them.
#
# Requires: claude (CLI), node, and a built symora binary. Install the target
# repo's language server first (`symora doctor <lang>`) or the WITH arm's
# LSP-backed tools won't resolve. Runs that crash (no `result` event) or whose
# WITH arm never exposed the symora MCP tools are marked failed and excluded
# from the medians — never counted as legitimate zeros.
set -euo pipefail

REPO="${1:?usage: run-ab.sh <repo-dir> <task-file> [runs] [out-dir]}"
TASK_FILE="${2:?missing task file}"
RUNS="${3:-5}"
OUT="${4:-/tmp/symora-agent-eval}"
SYMORA_BIN="${SYMORA_BIN:?set SYMORA_BIN to the symora binary under test}"
MODEL="${EVAL_MODEL:-claude-opus-5-5}"
EFFORT="${EVAL_EFFORT:-high}"

REPO="$(cd "$REPO" && pwd)"
TASK="$(cat "$TASK_FILE")"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$OUT"

for tool in claude node "$SYMORA_BIN"; do
  command -v "$tool" >/dev/null 2>&1 || [ -x "$tool" ] || {
    echo "required tool not found: $tool" >&2
    exit 1
  }
done

with_cfg="$OUT/mcp-with.json"
without_cfg="$OUT/mcp-without.json"
node -e 'process.stdout.write(JSON.stringify({mcpServers:{symora:{command:process.argv[1],args:["mcp","serve","--profile","read-only"]}}})+"\n")' "$SYMORA_BIN" >"$with_cfg"
printf '{"mcpServers":{}}\n' >"$without_cfg"

# Build the WITH arm's index against the binary under test, and confirm the
# repo's language server is present. A missing server invalidates the cell.
if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
  echo "Benchmark requires a clean, committed target repository" >&2
  exit 1
fi
( cd "$REPO" && "$SYMORA_BIN" search index build ) >"$OUT/index-build.json"
git -C "$REPO" rev-parse HEAD >"$OUT/revision.txt"
"$SYMORA_BIN" --version >"$OUT/symora-version.txt"
claude --version >"$OUT/claude-version.txt"
printf 'model=%s\neffort=%s\n' "$MODEL" "$EFFORT" >"$OUT/model.txt"
cp "$TASK_FILE" "$OUT/task.txt"

run_arm() {
  local arm="$1" cfg="$2" i raw metrics
  for i in $(seq 1 "$RUNS"); do
    raw="$OUT/${arm}-run${i}.jsonl"
    metrics="$OUT/${arm}-run${i}.json"
    echo "  [$arm] run $i/$RUNS" >&2
    # Do not suppress failures into a silent empty file: parse-run.mjs flags a
    # transcript with no `result` event as failed, and aggregate.mjs drops it.
    ( cd "$REPO" && claude -p "$TASK" \
        --output-format stream-json --verbose \
        --permission-mode plan \
        --model "$MODEL" --effort "$EFFORT" \
        --strict-mcp-config --mcp-config "$cfg" ) >"$raw" 2>>"$OUT/${arm}-run${i}.err" || true
    node "$SCRIPT_DIR/parse-run.mjs" <"$raw" >"$metrics"
    if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
      echo "Target repository changed during evaluation; stop and inspect it before continuing" >&2
      exit 1
    fi

    # Validity guard from the methodology: a WITH run whose system/init event
    # never exposed the symora MCP tools (parse-run's structured
    # `symora_tools_exposed`) measures "no MCP", not symora — mark it failed
    # so the aggregator excludes it from the medians. Guarded so a patch
    # failure degrades to a logged warning, not an aborted benchmark.
    if [ "$arm" = "with" ] && ! node -e 'const fs=require("fs");process.exit(JSON.parse(fs.readFileSync(process.argv[1],"utf8")).symora_tools_exposed ? 0 : 1)' "$metrics"; then
      echo "    FAILED: with-arm run $i never exposed mcp__symora__ tools (check SYMORA_BIN / MCP config) — excluded from medians" >&2
      node -e 'const fs=require("fs");const f=process.argv[1];const m=JSON.parse(fs.readFileSync(f,"utf8"));m.failed=true;m.failed_reason="mcp_not_exposed";fs.writeFileSync(f,JSON.stringify(m)+"\n");' "$metrics" \
        || echo "    WARN: could not mark $metrics failed" >&2
    fi
  done
}

echo "repo=$REPO runs=$RUNS out=$OUT" >&2
echo "WITH symora:" >&2
run_arm with "$with_cfg"
echo "WITHOUT symora:" >&2
run_arm without "$without_cfg"

node "$SCRIPT_DIR/aggregate.mjs" "$OUT" "$RUNS"
