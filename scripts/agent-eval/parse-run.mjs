#!/usr/bin/env node
// Parse a `claude -p --output-format stream-json --verbose` transcript into
// the metrics the A/B benchmark reports. Reads the transcript on stdin (one
// JSON object per line) and prints a one-line JSON summary on stdout.
//
// The one correctness trap this guards against: token totals are SUMMED from
// every per-turn assistant `usage`, NOT taken from the final `result.usage`
// (which in Claude Code reports the LAST turn only and undercounts massively).
// Run `node parse-run.mjs --selftest` to verify this logic against a
// synthetic transcript with no external dependencies.

const GREP_BASH = /\b(grep|rg|ag|ack|find)\b/;

function emptyMetrics() {
  return {
    // Context tokens: input + output + cache_creation + cache_read, summed
    // per assistant turn. A context-pressure proxy (cache_read re-counts the
    // conversation each turn), NOT dollars — dollars come from cost_usd.
    tokens: 0,
    output_tokens: 0,
    symora_calls: 0,
    read: 0,
    grep: 0, // dedicated Grep tool + Bash invocations that shell out to grep/find
    bash: 0,
    glob: 0,
    task: 0,
    other_tools: 0,
    turns: 0,
    duration_ms: null,
    cost_usd: null,
    // True when the system/init event's tool list contains mcp__symora__*
    // — i.e. the MCP server actually launched and was exposed to the agent,
    // independent of whether the agent chose to call it.
    symora_tools_exposed: false,
  };
}

function addUsage(m, usage) {
  if (!usage) return;
  const out = usage.output_tokens || 0;
  m.output_tokens += out;
  m.tokens +=
    (usage.input_tokens || 0) +
    out +
    (usage.cache_creation_input_tokens || 0) +
    (usage.cache_read_input_tokens || 0);
}

function classifyTool(m, name, input) {
  if (typeof name === "string" && name.startsWith("mcp__symora__")) {
    m.symora_calls += 1;
  } else if (name === "Read") {
    m.read += 1;
  } else if (name === "Grep") {
    m.grep += 1;
  } else if (name === "Glob") {
    m.glob += 1;
  } else if (name === "Task") {
    m.task += 1;
  } else if (name === "Bash") {
    m.bash += 1;
    // A Bash call that shells out to a search tool is a grep-equivalent
    // fallback — count it as such so the fall-back-to-grep rate is honest.
    const cmd = (input && (input.command || input.cmd)) || "";
    if (GREP_BASH.test(cmd)) m.grep += 1;
  } else if (typeof name === "string") {
    m.other_tools += 1;
  }
}

export function parse(lines) {
  const m = emptyMetrics();
  let sawResult = false;
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let ev;
    try {
      ev = JSON.parse(trimmed);
    } catch {
      continue; // tolerate non-JSON noise lines
    }

    if (ev.type === "system" && ev.subtype === "init") {
      const tools = Array.isArray(ev.tools) ? ev.tools : [];
      if (tools.some((t) => typeof t === "string" && t.startsWith("mcp__symora__"))) {
        m.symora_tools_exposed = true;
      }
    } else if (ev.type === "assistant" && ev.message) {
      m.turns += 1;
      addUsage(m, ev.message.usage);
      for (const block of ev.message.content || []) {
        if (block && block.type === "tool_use") {
          classifyTool(m, block.name, block.input);
        }
      }
    } else if (ev.type === "result") {
      sawResult = true;
      // `result.usage` is the LAST turn only — deliberately NOT summed here.
      // Cumulative fields, however, are authoritative.
      if (typeof ev.duration_ms === "number") m.duration_ms = ev.duration_ms;
      if (typeof ev.total_cost_usd === "number") m.cost_usd = ev.total_cost_usd;
      if (typeof ev.num_turns === "number") m.num_turns_reported = ev.num_turns;
    }
  }
  // The fall-back-to-grep signal: file/text scanning the tool was meant to
  // displace. Reported alongside symora_calls so an arm that simply ignored
  // symora is distinguishable from one where symora sufficed.
  m.read_grep_total = m.read + m.grep;
  // A run with no terminating `result` event (or zero turns) crashed, hit a
  // budget cap, or never started the agent. It is NOT a legitimate zero — flag
  // it so the aggregator drops it instead of biasing the medians toward zero.
  // `failed_reason` vocabulary is owned here: "no_result_or_zero_turns" (this
  // parser) and "mcp_not_exposed" (run-ab.sh's WITH-arm validity guard).
  m.failed = !sawResult || m.turns === 0;
  if (m.failed) m.failed_reason = "no_result_or_zero_turns";
  return m;
}

function selftest() {
  // An init event exposing the symora tools, then two assistant turns:
  // turn 1 calls a symora tool, turn 2 falls back to Read + a Bash grep.
  // result carries only the LAST turn's usage.
  const transcript = [
    JSON.stringify({
      type: "system",
      subtype: "init",
      tools: ["Read", "Bash", "mcp__symora__get_impact", "mcp__symora__find_references"],
    }),
    JSON.stringify({
      type: "assistant",
      message: {
        usage: {
          input_tokens: 100,
          output_tokens: 50,
          cache_read_input_tokens: 1000,
        },
        content: [{ type: "tool_use", name: "mcp__symora__get_impact", input: {} }],
      },
    }),
    JSON.stringify({
      type: "assistant",
      message: {
        usage: { input_tokens: 200, output_tokens: 80, cache_creation_input_tokens: 10 },
        content: [
          { type: "tool_use", name: "Read", input: { file_path: "a.rs" } },
          { type: "tool_use", name: "Bash", input: { command: "grep -r foo src/" } },
        ],
      },
    }),
    JSON.stringify({
      type: "result",
      usage: { input_tokens: 200, output_tokens: 80 }, // last-turn only — must be ignored
      duration_ms: 4200,
      total_cost_usd: 0.0123,
      num_turns: 2,
    }),
  ];

  const m = parse(transcript);
  const expect = (label, got, want) => {
    if (got !== want) {
      console.error(`selftest FAIL: ${label} = ${got}, expected ${want}`);
      process.exit(1);
    }
  };
  // tokens = (100+50+1000) + (200+80+10) = 1150 + 290 = 1440 — summed, NOT 280.
  expect("tokens (summed per-turn)", m.tokens, 1440);
  expect("output_tokens", m.output_tokens, 130);
  expect("symora_calls", m.symora_calls, 1);
  expect("read", m.read, 1);
  expect("grep (Bash-grep counts)", m.grep, 1);
  expect("bash", m.bash, 1);
  expect("read_grep_total", m.read_grep_total, 2);
  expect("turns", m.turns, 2);
  expect("duration_ms", m.duration_ms, 4200);
  expect("cost_usd", m.cost_usd, 0.0123);
  expect("failed (complete run)", m.failed, false);
  expect("symora_tools_exposed (init listed them)", m.symora_tools_exposed, true);

  // An empty/crashed transcript (no result event) is flagged failed, not a
  // legitimate zero — this is what stops the aggregator biasing medians.
  expect("failed (empty transcript)", parse([]).failed, true);
  expect("failed_reason (empty transcript)", parse([]).failed_reason, "no_result_or_zero_turns");
  expect("failed (no result event)", parse([transcript[1]]).failed, true);

  // Exposure comes only from the init event's tool list — an init without
  // the symora tools (the WITHOUT arm, or a failed MCP launch) stays false
  // even though the literal string appears elsewhere in the transcript.
  const bareInit = JSON.stringify({ type: "system", subtype: "init", tools: ["Read", "Bash"] });
  expect("symora_tools_exposed (bare init)", parse([bareInit, transcript[1]]).symora_tools_exposed, false);

  console.log("selftest OK — token summing, tool classification, failed-run and exposure detection verified");
}

async function readStdin() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  return Buffer.concat(chunks).toString("utf8").split("\n");
}

const isMain = import.meta.url === `file://${process.argv[1]}`;
if (isMain) {
  if (process.argv.includes("--selftest")) {
    selftest();
  } else {
    const lines = await readStdin();
    process.stdout.write(JSON.stringify(parse(lines)) + "\n");
  }
}
