//! Cross-surface parity.
//!
//! Symora's invariants require the same command to mean the same thing on
//! every surface: CLI ↔ MCP (one shared command layer) and daemon ↔
//! direct execution (one shared service layer). These tests machine-check
//! both, table-driven so new commands add one row, not one test.
//!
//! Payloads are compared as parsed `serde_json::Value`, never as bytes —
//! formatting is a transport concern, meaning is the contract.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const SYMORA: &str = env!("CARGO_BIN_EXE_symora");

struct ParityCase {
    /// MCP tool name.
    tool: &'static str,
    /// MCP `arguments` payload.
    arguments: &'static str,
    /// Equivalent CLI argv (without the binary).
    cli: &'static [&'static str],
}

/// Hermetic cases only: no LSP server, no daemon index required. Error
/// payloads count as payloads — handled failures must also agree.
const CASES: &[ParityCase] = &[
    ParityCase {
        tool: "get_project_overview",
        arguments: "{}",
        cli: &["map", "summary", "--limit", "10"],
    },
    ParityCase {
        tool: "search_symbols",
        arguments: r#"{"query":""}"#,
        cli: &["search", "symbols", ""],
    },
    ParityCase {
        tool: "search_content",
        arguments: r#"{"query":""}"#,
        cli: &["search", "content", ""],
    },
    ParityCase {
        tool: "get_diagnostics",
        arguments: r#"{"file":"src/main.rs","severity":"bogus"}"#,
        cli: &["diagnostics", "src/main.rs", "--severity", "bogus"],
    },
];

fn run_cli(args: &[&str]) -> Vec<Value> {
    let output = Command::new(SYMORA)
        .args(args)
        .args(["--format", "compact"])
        .env("SYMORA_NO_DAEMON", "1")
        .stderr(Stdio::null())
        .output()
        .expect("run symora CLI");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("CLI emits valid JSON"))
        .collect()
}

fn run_mcp_tool(tool: &str, arguments: &str) -> Vec<Value> {
    let mut child = Command::new(SYMORA)
        .args(["mcp", "serve"])
        .env("SYMORA_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn symora mcp serve");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"symora-tests","version":"1"}}}}}}"#
        )
        .unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let call_response: Value =
        serde_json::from_str(stdout.lines().nth(1).expect("tools/call response line"))
            .expect("valid JSON-RPC response");

    call_response["result"]["content"][0]["text"]
        .as_str()
        .expect("text content")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("tool emits valid JSON"))
        .collect()
}

#[test]
fn cli_and_mcp_emit_identical_payloads() {
    for case in CASES {
        let cli = run_cli(case.cli);
        let mcp = run_mcp_tool(case.tool, case.arguments);
        assert_eq!(
            cli, mcp,
            "CLI {:?} and MCP tool '{}' diverged",
            case.cli, case.tool,
        );
        assert!(
            !cli.is_empty(),
            "parity case {:?} produced no output — the comparison is vacuous",
            case.cli,
        );
    }
}

fn run_at(root: &std::path::Path, args: &[&str], direct: bool) -> Vec<Value> {
    let mut command = Command::new(SYMORA);
    command
        .current_dir(root)
        .args(args)
        .args(["--format", "compact"]);
    if direct {
        command.env("SYMORA_NO_DAEMON", "1");
    } else {
        command.env_remove("SYMORA_NO_DAEMON");
    }
    let output = command.output().expect("run fixture command");
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid output for {args:?}: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        output.status.success(),
        "fixture command {args:?} failed: {payload}"
    );
    vec![payload]
}

/// Whether a payload carries a degradation marker, at any depth.
///
/// `indexing` states how far along the language server's workspace analysis
/// was when the answer was computed. Two backends warmed at different times
/// legitimately report different states, so a payload carrying one says
/// nothing about parity — it is a snapshot of the run, not of the meaning.
fn discloses_degradation(payload: &[Value]) -> bool {
    fn walk(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.contains_key("indexing") || map.values().any(walk),
            Value::Array(items) => items.iter().any(walk),
            _ => false,
        }
    }
    payload.iter().any(walk)
}

/// Poll one backend until it stops disclosing degradation, and return the
/// payload it settled on.
fn quiesced(
    backend: &str,
    args: &[&str],
    run: impl Fn(&[&str]) -> Vec<Value>,
    deadline: Instant,
) -> Vec<Value> {
    loop {
        let payload = run(args);
        if !discloses_degradation(&payload) {
            return payload;
        }
        assert!(
            Instant::now() < deadline,
            "{backend} never reached quiescence for {args:?}; parity is undefined while \
             workspace analysis is still running",
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

const QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Daemon ↔ direct parity for LSP-backed commands. Needs rust-analyzer
/// and a startable daemon, so it is opt-in:
/// `cargo test --test parity -- --ignored`.
///
/// Each backend is warmed to quiescence before the comparison, because the
/// invariant is only defined once both have an answer to give: while a
/// workspace is still being analysed the two sides are looking at different
/// amounts of the same project, and that difference is disclosed on
/// purpose. Once quiescent, the payloads are compared whole — no field is
/// exempted, so a real divergence cannot hide behind an allowance.
#[test]
#[ignore = "requires rust-analyzer and a daemon-capable environment"]
fn daemon_and_direct_emit_identical_payloads() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"parity_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn add(a: u32, b: u32) -> u32 { a + b }\npub fn answer() -> u32 { add(20, 22) }\n",
    )
    .unwrap();
    struct StopDaemon<'a>(&'a std::path::Path);
    impl Drop for StopDaemon<'_> {
        fn drop(&mut self) {
            let _ = Command::new(SYMORA)
                .current_dir(self.0)
                .args(["daemon", "stop"])
                .output();
        }
    }
    let _stop = StopDaemon(root);
    let cases: &[(&[&str], Option<&str>)] = &[
        (&["symbols", "src/lib.rs", "--depth", "1"], Some("document")),
        (&["refs", "src/lib.rs:1:8", "--limit", "5"], None),
        (&["diagnostics", "src/lib.rs"], None),
    ];

    for (args, server_backend) in cases {
        let deadline = Instant::now() + QUIESCENCE_TIMEOUT;
        let direct = quiesced("direct", args, |args| run_at(root, args, true), deadline);
        let daemon = quiesced("daemon", args, |args| run_at(root, args, false), deadline);

        // A payload is always non-empty — an error object is a payload — so
        // emptiness is not what makes a case vacuous. Two sides that failed
        // alike are equal: without rust-analyzer, `refs` and `diagnostics`
        // return `server_not_installed` on each. So are two fallback answers,
        // and a reference case that resolved to no symbol compares two
        // identical nothings. None of these proves anything about parity.
        assert!(
            !direct.is_empty(),
            "parity case {args:?} produced no output — the comparison is vacuous",
        );
        assert!(
            !direct.iter().any(|payload| payload.get("error").is_some()),
            "parity case {args:?} failed — the comparison is vacuous: {direct:?}",
        );
        if let Some(backend) = server_backend {
            assert!(
                direct.iter().all(|payload| payload["backend"] == *backend),
                "parity case {args:?} was not answered by the language server — the \
                 comparison is vacuous: {direct:?}",
            );
        }
        assert!(
            !direct
                .iter()
                .any(|payload| payload["target"]["anchor_status"] == "not_a_symbol"),
            "parity case {args:?} anchored no symbol — the comparison is vacuous",
        );
        assert_eq!(direct, daemon, "daemon and direct diverged for {args:?}");
    }
}

/// The quiescence gate is only meaningful if it actually recognises the
/// marker — a detector that always answered "clean" would turn the wait
/// into a no-op and put the flaky comparison right back.
#[test]
fn degradation_is_detected_at_any_depth() {
    let top: Value = serde_json::from_str(r#"{"count":0,"indexing":"timed_out"}"#).unwrap();
    let nested: Value =
        serde_json::from_str(r#"{"refs":{"total":0,"indexing":"timed_out"}}"#).unwrap();
    let in_array: Value = serde_json::from_str(r#"{"items":[{"indexing":"timed_out"}]}"#).unwrap();
    let clean: Value = serde_json::from_str(r#"{"count":1,"items":[{"line":3}]}"#).unwrap();

    assert!(discloses_degradation(&[top]));
    assert!(discloses_degradation(&[nested]));
    assert!(discloses_degradation(&[in_array]));
    assert!(!discloses_degradation(&[clean]));
    assert!(!discloses_degradation(&[]));
}
