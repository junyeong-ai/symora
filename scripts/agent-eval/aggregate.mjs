#!/usr/bin/env node
// Aggregate per-run metrics from run-ab.sh into per-arm medians + the
// with-vs-without deltas. Headlines are medians (run-to-run variance is
// large — never conclude from a single run); min/max are kept so the spread
// stays visible. Run `node aggregate.mjs --selftest` to verify the math.

import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const FIELDS = ["tokens", "read", "grep", "read_grep_total", "symora_calls", "turns", "duration_ms", "cost_usd"];

function median(xs) {
  const v = xs.filter((x) => typeof x === "number").sort((a, b) => a - b);
  if (v.length === 0) return null;
  const mid = Math.floor(v.length / 2);
  return v.length % 2 ? v[mid] : (v[mid - 1] + v[mid]) / 2;
}

function summarizeArm(runs) {
  const out = {};
  for (const f of FIELDS) {
    const xs = runs.map((r) => r[f]).filter((x) => typeof x === "number");
    out[f] = { median: median(xs), min: xs.length ? Math.min(...xs) : null, max: xs.length ? Math.max(...xs) : null };
  }
  return out;
}

function pctDelta(withMed, withoutMed) {
  if (typeof withMed !== "number" || typeof withoutMed !== "number" || withoutMed === 0) return null;
  return Math.round(((withMed - withoutMed) / withoutMed) * 1000) / 10; // one decimal, %
}

export function aggregate(withRuns, withoutRuns) {
  const eligible = (runs) => runs.filter((run) => !run.failed && run.correct === true);
  const counts = (runs) => ({
    ok: eligible(runs).length,
    failed: runs.filter((run) => run.failed).length,
    incorrect: runs.filter((run) => !run.failed && run.correct === false).length,
    unjudged: runs.filter((run) => !run.failed && typeof run.correct !== "boolean").length,
  });
  const okWith = eligible(withRuns);
  const okWithout = eligible(withoutRuns);
  const withSummary = summarizeArm(okWith);
  const withoutSummary = summarizeArm(okWithout);
  const deltas = {};
  for (const f of FIELDS) {
    deltas[f] = pctDelta(withSummary[f].median, withoutSummary[f].median);
  }
  return {
    runs: {
      with: counts(withRuns),
      without: counts(withoutRuns),
    },
    attempts: { with: summarizeArm(withRuns), without: summarizeArm(withoutRuns) },
    with: withSummary,
    without: withoutSummary,
    delta_pct: deltas,
  };
}

function loadRuns(dir, arm, runs) {
  const out = [];
  for (let i = 1; i <= runs; i++) {
    const path = join(dir, `${arm}-run${i}.json`);
    try {
      out.push(JSON.parse(readFileSync(path, "utf8")));
    } catch {
      out.push({ failed: true, failed_reason: "missing_or_invalid_metrics" });
    }
  }
  return out;
}

function selftest() {
  const withRuns = [
    { tokens: 100, read: 0, correct: true },
    { tokens: 120, read: 0, correct: true },
    { tokens: 110, read: 1, correct: true },
    { tokens: 1, read: 0, correct: false },
    { tokens: 1, read: 0 },
    { tokens: 0, read: 0, failed: true }, // crashed run — must be excluded
  ];
  const withoutRuns = [{ tokens: 200, read: 4, correct: true }, { tokens: 240, read: 6, correct: true }, { tokens: 220, read: 5, correct: true }];
  const agg = aggregate(withRuns, withoutRuns);
  const fail = (msg) => {
    console.error("selftest FAIL:", msg);
    process.exit(1);
  };
  if (agg.with.tokens.median !== 110) fail(`with tokens median ${agg.with.tokens.median} != 110 (failed run leaked in?)`);
  if (agg.runs.with.ok !== 3 || agg.runs.with.failed !== 1) fail(`with run accounting ${JSON.stringify(agg.runs.with)}`);
  if (agg.runs.with.incorrect !== 1 || agg.runs.with.unjudged !== 1) fail("correctness accounting");
  const unjudged = aggregate([{ tokens: 1 }], [{ tokens: 2 }]);
  if (unjudged.delta_pct.tokens !== null) fail("unjudged runs produced a comparison");
  if (agg.without.tokens.median !== 220) fail(`without tokens median ${agg.without.tokens.median} != 220`);
  if (agg.delta_pct.tokens !== -50) fail(`tokens delta ${agg.delta_pct.tokens} != -50`);
  if (agg.without.read.median !== 5) fail(`without read median ${agg.without.read.median} != 5`);
  console.log("selftest OK — median/delta math and failed-run exclusion verified");
}

const isMain = import.meta.url === `file://${process.argv[1]}`;
if (isMain) {
  if (process.argv.includes("--selftest")) {
    selftest();
  } else {
    const dir = process.argv[2] || ".";
    const runs = Number(process.argv[3] || readdirSync(dir).filter((f) => f.startsWith("with-run")).length || 1);
    const agg = aggregate(loadRuns(dir, "with", runs), loadRuns(dir, "without", runs));
    process.stdout.write(JSON.stringify(agg, null, 2) + "\n");
  }
}
