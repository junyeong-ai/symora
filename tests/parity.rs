//! Cross-surface parity.
//!
//! Symora's invariants require the same command to mean the same thing on
//! every surface: CLI ↔ MCP (one shared command layer) and daemon ↔
//! direct execution (one shared service layer). These tests machine-check
//! both, table-driven so new commands add one row, not one test.
//!
//! Payloads are compared as parsed `serde_json::Value`, never as bytes —
//! formatting is a transport concern, meaning is the contract.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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
        tool: "search_content",
        arguments: r#"{"query":"source","path":[".claude/skills/symora/SKILL.md"]}"#,
        cli: &[
            "search",
            "content",
            "source",
            "--path",
            ".claude/skills/symora/SKILL.md",
        ],
    },
    ParityCase {
        tool: "get_diff_impact",
        arguments: r#"{"revision":"symora_nonexistent_test_revision"}"#,
        cli: &["diff-impact", "symora_nonexistent_test_revision"],
    },
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

struct McpSession {
    child: Child,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpSession {
    fn new(root: Option<&Path>) -> Self {
        let mut command = Command::new(SYMORA);
        command
            .args(["mcp", "serve"])
            .env("SYMORA_NO_DAEMON", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(root) = root {
            command.current_dir(root);
        }
        let mut child = command.spawn().expect("spawn symora mcp serve");
        let reader = BufReader::new(child.stdout.take().expect("MCP stdout"));
        let mut session = Self {
            child,
            reader,
            next_id: 1,
        };
        session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "symora-tests", "version": "1"},
            }),
        );
        session.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        session
    }

    fn send(&mut self, message: Value) {
        let stdin = self.child.stdin.as_mut().expect("MCP stdin");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).expect("read MCP response") > 0,
                "MCP closed before answering {method}",
            );
            let response: Value = serde_json::from_str(&line).expect("valid JSON-RPC response");
            if response.get("id").is_none() {
                assert!(
                    response["method"].is_string(),
                    "invalid MCP notification: {response}"
                );
                continue;
            }
            assert_eq!(response["id"], id, "MCP response ID for {method}");
            assert!(
                response.get("error").is_none(),
                "MCP {method} failed: {response}"
            );
            return response.get("result").expect("MCP result").clone();
        }
    }

    fn call(&mut self, tool: &str, arguments: &str) -> Vec<Value> {
        let arguments: Value = serde_json::from_str(arguments).expect("tool arguments");
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        result["content"][0]["text"]
            .as_str()
            .expect("text content")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("tool emits valid JSON"))
            .collect()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

fn run_mcp_tool(tool: &str, arguments: &str) -> Vec<Value> {
    McpSession::new(None).call(tool, arguments)
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

fn run_daemon_cli(root: &Path, args: &[&str]) -> Vec<Value> {
    let mut command = Command::new(SYMORA);
    command
        .current_dir(root)
        .args(args)
        .args(["--format", "compact"])
        .env_remove("SYMORA_NO_DAEMON");
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
    mut run: impl FnMut(&[&str]) -> Vec<Value>,
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
             workspace analysis is still running; last payload: {payload:?}",
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

const QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Daemon CLI ↔ persistent direct MCP parity for LSP-backed commands.
/// Needs rust-analyzer and a startable daemon, so it is opt-in:
/// `cargo test --test parity -- --ignored`.
///
/// Both backends retain their language server while reaching quiescence.
/// MCP runs the CLI's command layer. Once workspace analysis settles, compare
/// entire payloads without exempting any fields or degradation markers.
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
    let mut direct_session = McpSession::new(Some(root));
    let cases = &[
        (
            ParityCase {
                tool: "list_file_symbols",
                arguments: r#"{"file":"src/lib.rs","depth":1}"#,
                cli: &["symbols", "src/lib.rs", "--depth", "1"],
            },
            Some("document"),
        ),
        (
            ParityCase {
                tool: "find_references",
                arguments: r#"{"file":"src/lib.rs","line":1,"column":8,"limit":5}"#,
                cli: &["refs", "src/lib.rs:1:8", "--limit", "5"],
            },
            None,
        ),
        (
            ParityCase {
                tool: "get_diagnostics",
                arguments: r#"{"file":"src/lib.rs"}"#,
                cli: &["diagnostics", "src/lib.rs"],
            },
            None,
        ),
    ];

    for (case, server_backend) in cases {
        let args = case.cli;
        let deadline = Instant::now() + QUIESCENCE_TIMEOUT;
        let direct = quiesced(
            "direct MCP",
            args,
            |_| direct_session.call(case.tool, case.arguments),
            deadline,
        );
        let daemon = quiesced(
            "daemon CLI",
            args,
            |args| run_daemon_cli(root, args),
            deadline,
        );

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

#[test]
#[cfg(unix)]
fn unconfirmed_rename_is_refused_by_cli_mcp_and_daemon() {
    if !Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
    {
        eprintln!("rename protocol fixture requires python3");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("main.go");
    let original = "package main\nfunc abc() {}\nfunc caller() { abc() }\n";
    std::fs::write(&source, original).unwrap();
    std::fs::write(root.path().join("go.mod"), "module example.com/fixture\n").unwrap();
    std::fs::create_dir(root.path().join(".symora")).unwrap();
    let server =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp/code_action_server.py");
    let config = json!({"lsp":{"servers":{"go":{
        "command":"python3", "args":[server, source, "unconfirmed-rename"]
    }}}});
    std::fs::write(
        root.path().join(".symora/config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    let mut expected = None;
    for direct in [true, false] {
        let mut command = Command::new(SYMORA);
        command.current_dir(root.path()).args([
            "rename",
            "main.go:3:17",
            "Renamed",
            "--format",
            "compact",
        ]);
        if direct {
            command.env("SYMORA_NO_DAEMON", "1");
        } else {
            command.env_remove("SYMORA_NO_DAEMON");
        }
        let output = command.output().unwrap();
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(!output.status.success(), "{result}");
        assert_eq!(result["error"]["code"], "precondition_failed", "{result}");
        if let Some(ref expected) = expected {
            assert_eq!(&result, expected);
        } else {
            expected = Some(result);
        }
        assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
    }
    let mut session = McpSession::new(Some(root.path()));
    let result = session.request("tools/call", json!({
        "name":"rename_symbol", "arguments":{"file":"main.go", "line":3, "column":17, "new_name":"Renamed"}
    }));
    assert_eq!(result["isError"], true, "{result}");
    let payload: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(Some(&payload), expected.as_ref());
    assert_eq!(result["structuredContent"], payload);
    assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
}
