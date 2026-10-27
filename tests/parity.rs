//! Cross-surface parity.
//!
//! Symora's invariants require the same command to mean the same thing on
//! every surface: CLI ↔ MCP (one shared command layer) and daemon ↔
//! direct execution (one shared service layer). These tests machine-check
//! both, table-driven so new commands add one row, not one test.
//!
//! Payloads are compared as parsed `serde_json::Value`, never as bytes —
//! formatting is a transport concern, meaning is the contract.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;
mod mcp_session;

use common::{no_global_config, symora};
use mcp_session::McpSession;

struct ParityCase {
    /// MCP tool name.
    tool: &'static str,
    /// MCP `arguments` payload.
    arguments: &'static str,
    /// Equivalent CLI argv (without the binary).
    cli: &'static [&'static str],
    /// The error code both surfaces answer with; `None` where they succeed.
    error: Option<&'static str>,
}

impl ParityCase {
    fn arguments(&self) -> Value {
        serde_json::from_str(self.arguments).expect("tool arguments")
    }

    /// Each payload answers as the case expects: with `error`'s code, or with
    /// none where it is `None`. Two surfaces that fail alike compare equal,
    /// so their agreement says nothing about a case meant to succeed.
    fn assert_answered(&self, payloads: &[Value]) {
        for payload in payloads {
            assert_eq!(
                payload["error"]["code"].as_str(),
                self.error,
                "parity case {:?} answered unexpectedly: {payload}",
                self.cli,
            );
        }
    }
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
        error: None,
    },
    ParityCase {
        tool: "get_diff_impact",
        arguments: r#"{"revision":"symora_nonexistent_test_revision"}"#,
        cli: &["diff-impact", "symora_nonexistent_test_revision"],
        error: Some("invalid_argument"),
    },
    ParityCase {
        tool: "get_project_overview",
        arguments: "{}",
        cli: &["map", "summary", "--limit", "10"],
        error: None,
    },
    ParityCase {
        tool: "search_symbols",
        arguments: r#"{"query":""}"#,
        cli: &["search", "symbols", ""],
        error: Some("invalid_argument"),
    },
    ParityCase {
        tool: "search_content",
        arguments: r#"{"query":""}"#,
        cli: &["search", "content", ""],
        error: Some("invalid_argument"),
    },
    ParityCase {
        tool: "search_symbols",
        arguments: r#"{"query":"x","path":["symora_missing_parity_path"]}"#,
        cli: &[
            "search",
            "symbols",
            "x",
            "--path",
            "symora_missing_parity_path",
        ],
        error: Some("not_found"),
    },
    ParityCase {
        tool: "get_diagnostics",
        arguments: r#"{"file":"src/main.rs","severity":"bogus"}"#,
        cli: &["diagnostics", "src/main.rs", "--severity", "bogus"],
        error: Some("invalid_argument"),
    },
];

fn run_cli(args: &[&str]) -> Vec<Value> {
    let output = symora()
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

fn run_mcp_tool(case: &ParityCase) -> Vec<Value> {
    McpSession::new(None).call(case.tool, case.arguments())
}

#[test]
fn cli_and_mcp_emit_identical_payloads() {
    for case in CASES {
        let cli = run_cli(case.cli);
        let mcp = run_mcp_tool(case);
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
        case.assert_answered(&cli);
    }
}

/// A daemon only one test talks to. A copy of the binary is an installation
/// of its own, so its daemon starts from this suite's isolated environment
/// instead of joining one a manual run of the same build left behind, and
/// two tests never stop each other's. Dropping it stops the daemon.
struct IsolatedDaemon {
    exe: PathBuf,
    home: tempfile::TempDir,
}

impl IsolatedDaemon {
    fn for_test(name: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("parity-daemon")
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let daemon = Self {
            exe: dir.join("symora"),
            home: tempfile::tempdir().unwrap(),
        };
        if daemon.exe.exists() {
            // A copy written over one that has run is new code in a file the
            // kernel already verified, and macOS kills whatever executes it:
            // replace the file, not its contents.
            std::fs::remove_file(&daemon.exe).unwrap();
        }
        // Copy in a child so this process never holds the executable open for
        // writing, which a concurrent fork could inherit (ETXTBSY on Linux).
        let copied = Command::new("cp")
            .arg(env!("CARGO_BIN_EXE_symora"))
            .arg(&daemon.exe)
            .status()
            .unwrap();
        assert!(copied.success());
        daemon
    }

    /// The daemon keeps its socket and pid files under its home, so it gets
    /// one of its own rather than the user's, short enough for a socket's
    /// path; rustup and cargo, which the language server runs, still find the
    /// toolchains the user's home holds.
    fn command(&self, root: &Path) -> Command {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let toolchain = |var: &str, dir: &str| {
            std::env::var_os(var).map_or_else(|| home.join(dir), PathBuf::from)
        };
        let mut command = Command::new(&self.exe);
        command
            .current_dir(root)
            .env("HOME", self.home.path())
            .env("CARGO_HOME", toolchain("CARGO_HOME", ".cargo"))
            .env("RUSTUP_HOME", toolchain("RUSTUP_HOME", ".rustup"))
            .env("XDG_CONFIG_HOME", no_global_config())
            .env_remove("SYMORA_NO_DAEMON");
        command
    }

    /// Assert that this installation's daemon is serving from this test's own
    /// home: a command that could not reach one runs in process and agrees
    /// with the direct run without comparing anything, and one that served
    /// from another home left its socket and pid files there.
    fn assert_serving(&self) {
        let output = self
            .command(Path::new(env!("CARGO_TARGET_TMPDIR")))
            .args(["daemon", "status", "--format", "compact"])
            .output()
            .unwrap();
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["running"], true, "{status}");
        let socket = Path::new(status["socket_path"].as_str().unwrap_or_default());
        assert!(socket.starts_with(self.home.path()), "{status}");
    }

    fn stop(&self) {
        let _ = self
            .command(Path::new(env!("CARGO_TARGET_TMPDIR")))
            .args(["daemon", "stop"])
            .output();
    }
}

impl Drop for IsolatedDaemon {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_daemon_cli(daemon: &IsolatedDaemon, root: &Path, args: &[&str]) -> Vec<Value> {
    let mut command = daemon.command(root);
    command.args(args).args(["--format", "compact"]);
    let output = command.output().expect("run fixture command");
    let payload: Value = common::parse_response(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid output for {args:?}: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        output.status.success(),
        "fixture command {args:?} failed: {payload}"
    );
    daemon.assert_serving();
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
    let daemon = IsolatedDaemon::for_test("daemon_and_direct_emit_identical_payloads");
    let mut direct_session = McpSession::new(Some(root));
    let cases = &[
        (
            ParityCase {
                tool: "list_file_symbols",
                arguments: r#"{"file":"src/lib.rs","depth":1}"#,
                cli: &["symbols", "src/lib.rs", "--depth", "1"],
                error: None,
            },
            Some("document"),
        ),
        (
            ParityCase {
                tool: "find_references",
                arguments: r#"{"file":"src/lib.rs","line":1,"column":8,"limit":5}"#,
                cli: &["refs", "src/lib.rs:1:8", "--limit", "5"],
                error: None,
            },
            None,
        ),
        (
            ParityCase {
                tool: "get_diagnostics",
                arguments: r#"{"file":"src/lib.rs"}"#,
                cli: &["diagnostics", "src/lib.rs"],
                error: None,
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
            |_| direct_session.call(case.tool, case.arguments()),
            deadline,
        );
        let daemon = quiesced(
            "daemon CLI",
            args,
            |args| run_daemon_cli(&daemon, root, args),
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
        case.assert_answered(&direct);
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

    let daemon = IsolatedDaemon::for_test("unconfirmed_rename_is_refused_by_cli_mcp_and_daemon");
    let mut expected = None;
    for direct in [true, false] {
        let mut command = if direct {
            let mut command = symora();
            command
                .current_dir(root.path())
                .env("SYMORA_NO_DAEMON", "1");
            command
        } else {
            daemon.command(root.path())
        };
        command.args(["rename", "main.go:3:17", "Renamed", "--format", "compact"]);
        let output = command.output().unwrap();
        let result: Value = common::parse_response(&output.stdout).unwrap();
        assert!(!output.status.success(), "{result}");
        assert_eq!(result["error"]["code"], "precondition_failed", "{result}");
        if let Some(ref expected) = expected {
            assert_eq!(&result, expected);
        } else {
            expected = Some(result);
        }
        assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
        if !direct {
            daemon.assert_serving();
        }
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
