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
    // A documentation file: no language server is asked about it.
    ParityCase {
        tool: "get_diff_impact",
        arguments: r#"{"path":["README.md"]}"#,
        cli: &["diff-impact", "--path", "README.md"],
        error: None,
    },
    ParityCase {
        tool: "get_diff_impact",
        arguments: r#"{"path":["symora_missing_parity_path"]}"#,
        cli: &["diff-impact", "--path", "symora_missing_parity_path"],
        error: Some("not_found"),
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
    // A position past its file's end is refused before any server is asked,
    // so these answer alike with no server running.
    ParityCase {
        tool: "find_references",
        arguments: r#"{"file":"src/main.rs","line":99999}"#,
        cli: &["refs", "src/main.rs:99999"],
        error: Some("invalid_argument"),
    },
    ParityCase {
        tool: "get_hover",
        arguments: r#"{"file":"src/main.rs","line":1,"column":9999}"#,
        cli: &["hover", "src/main.rs:1:9999"],
        error: Some("invalid_argument"),
    },
    ParityCase {
        tool: "find_call_path",
        arguments: r#"{"file":"src/main.rs","line":1,"to":"src/main.rs:99999"}"#,
        cli: &["callees", "src/main.rs:1", "--to", "src/main.rs:99999"],
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

    /// Start the daemon before the test runs anything beside it. Where pipes
    /// cannot be made close-on-exec atomically (macOS), a process spawned
    /// while another thread makes one can inherit its writing end, and a
    /// daemon inherits it from the command that starts it and holds it for
    /// as long as it serves: the command the pipe belongs to never sees its
    /// output end, and the test stops the daemon only after that command
    /// returns.
    fn start(&self) {
        let output = self
            .command(Path::new(env!("CARGO_TARGET_TMPDIR")))
            .args(["daemon", "start", "--format", "compact"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "daemon start failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
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
        "pub fn add(a: u32, b: u32) -> u32 { a + b }\npub fn answer() -> u32 { add(20, 22) }\n\
         pub struct Counter(u32);\nimpl Counter {\n    pub fn get(&self) -> u32 { self.0 }\n}\n",
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

/// A first symbol search in a work tree builds the index it answers from
/// whichever way it runs — in process, through a daemon, or as an MCP tool —
/// so three copies of one project in a repository no index was built for
/// answer it alike, each from its own new index. The project's language
/// server cannot start, so an answer the index did not give would differ
/// from one it did. Each copy is a root of its own, so the three run at once.
#[test]
#[cfg(unix)]
fn a_first_search_builds_and_answers_alike_in_process_through_a_daemon_and_over_mcp() {
    // Copying the daemon's installation takes about as long as making the
    // repository, so the two overlap.
    let installing =
        std::thread::spawn(|| IsolatedDaemon::for_test("a_first_search_builds_and_answers_alike"));
    let repository = tempfile::tempdir().unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    assert!(init.success());
    let project = |name: &str| {
        let root = repository.path().join(name);
        std::fs::create_dir_all(root.join(".symora")).unwrap();
        std::fs::write(
            root.join("lib.rs"),
            "pub fn helper_alpha() {}\npub fn helper_beta() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".symora/config.toml"),
            "[lsp.servers.rust]\ncommand = \"/nonexistent/rust-analyzer-missing\"\n",
        )
        .unwrap();
        root
    };
    let args = ["search", "symbols", "helper"];
    let (direct_root, daemon_root, mcp_root) =
        (project("direct"), project("daemon"), project("mcp"));
    let daemon = installing.join().unwrap();
    daemon.start();

    let (direct, through_daemon, over_mcp) = std::thread::scope(|scope| {
        let direct = scope.spawn(|| {
            let output = symora()
                .args(args)
                .args(["--format", "compact"])
                .current_dir(&direct_root)
                .env("SYMORA_NO_DAEMON", "1")
                .stderr(Stdio::null())
                .output()
                .unwrap();
            vec![common::parse_response(&output.stdout).unwrap()]
        });
        let over_mcp = scope.spawn(|| {
            McpSession::new(Some(&mcp_root)).call("search_symbols", json!({"query": "helper"}))
        });
        let through_daemon = run_daemon_cli(&daemon, &daemon_root, &args);
        (
            direct.join().unwrap(),
            through_daemon,
            over_mcp.join().unwrap(),
        )
    });

    assert_eq!(direct[0]["count"], 2, "{direct:?}");
    assert!(
        direct[0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["backend"] == "index"),
        "{direct:?}"
    );
    assert_eq!(direct, through_daemon, "daemon and direct diverged");
    assert_eq!(direct, over_mcp, "MCP and the CLI diverged");
}

/// A symbol search spends its own budget on the index, whichever way it runs.
/// One that finds a build owning the index waits for it within
/// `lsp.timeout_secs`, and when the budget runs out first, the answer in
/// process and through a daemon alike leaves the index out, asks no language
/// server — one asked would say its server is not installed, one asked with
/// no budget left that it timed out — and names the build that finishes it
/// as its one remedy. That build has no budget in either mode: it waits
/// out the build owning the index, past the budget the searches spent, then
/// completes. One project serves both modes, so they meet the same index.
#[test]
#[cfg(unix)]
fn a_budget_spent_on_a_build_answers_alike_and_the_explicit_build_finishes_it() {
    let installing =
        std::thread::spawn(|| IsolatedDaemon::for_test("a_budget_spent_on_a_build_answers_alike"));
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(init.success());
    std::fs::create_dir(root.join(".symora")).unwrap();
    std::fs::write(root.join("lib.rs"), "pub fn helper_alpha() {}\n").unwrap();
    std::fs::write(
        root.join(".symora/config.toml"),
        "[lsp]\ntimeout_secs = 1\n\n[lsp.servers.rust]\ncommand = \
         \"/nonexistent/rust-analyzer-missing\"\n",
    )
    .unwrap();
    let daemon = installing.join().unwrap();
    daemon.start();
    let held = symora::infra::file_lock::FileLock::exclusive(&root.join(".symora/index.lock"))
        .unwrap()
        .expect("nothing else holds a fresh project's index");
    let direct_command = |args: &[&str]| {
        let mut command = symora();
        command
            .args(args)
            .args(["--format", "compact"])
            .current_dir(root)
            .env("SYMORA_NO_DAEMON", "1")
            .stderr(Stdio::null());
        command
    };

    let search = ["search", "symbols", "helper_alpha"];
    let started = Instant::now();
    let (direct, through_daemon) = std::thread::scope(|scope| {
        let direct = scope.spawn(|| {
            let output = direct_command(&search).output().unwrap();
            assert!(output.status.success());
            vec![common::parse_response(&output.stdout).unwrap()]
        });
        let through_daemon = run_daemon_cli(&daemon, root, &search);
        (direct.join().unwrap(), through_daemon)
    });
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the searches waited past their budget"
    );
    assert_eq!(direct, through_daemon, "daemon and direct diverged");
    let answer = &direct[0];
    assert_eq!(answer["count"], 0, "{answer}");
    assert!(
        answer["hints"][0]
            .as_str()
            .is_some_and(|hint| hint.contains("within this search's 1s budget")),
        "{answer}"
    );
    assert_eq!(
        answer["next_commands"],
        json!(["symora search index build"]),
        "nothing failed, so the build is the one remedy: {answer}"
    );
    assert_eq!(
        answer["coverage_gaps"],
        json!([{"language": "rust", "reason": "not_consulted"}]),
        "{answer}"
    );

    let build = ["search", "index", "build"];
    let mut daemon_build = daemon.command(root);
    daemon_build
        .args(build)
        .args(["--format", "compact"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let builds = [
        direct_command(&build)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
        daemon_build.spawn().unwrap(),
    ];
    std::thread::sleep(Duration::from_millis(1500));
    let builds = builds.map(|mut build| {
        let waited = build.try_wait().unwrap().is_none();
        (waited, build)
    });
    drop(held);
    for (waited, build) in builds {
        let output = build.wait_with_output().unwrap();
        let answer = common::parse_response(&output.stdout).unwrap();
        assert!(waited, "a build answered within the budget: {answer}");
        assert!(output.status.success(), "{answer}");
        assert_eq!(answer["status"], "completed", "{answer}");
    }
    daemon.assert_serving();
}

/// A query reads, and writes nothing into the tree it reads. Outside a work
/// tree no search starts a build either, so every read command — searches of
/// each kind, navigation, analysis, previews, status — leaves a plain
/// directory as it found it, in process and through a daemon alike: no
/// `.symora/` and no empty store. The language server cannot start, so each
/// command answers as far as it can without one. Each read has a directory
/// of its own, so they run at once.
#[test]
#[cfg(unix)]
fn a_read_leaves_a_tree_outside_a_work_tree_as_it_found_it() {
    const READS: &[&[&str]] = &[
        &["search", "symbols", "helper_alpha"],
        &["search", "symbols", "helper*"],
        &["search", "symbols", "Shape/area"],
        &["search", "symbols", "helper_alpha", "--workspace-symbols"],
        &["--deterministic", "search", "symbols", "helper_alpha"],
        &["search", "content", "helper_alpha"],
        &["search", "ast", "--lang", "rust", "(function_item)"],
        &["search", "index", "status"],
        &["symbols", "main.rs"],
        &["map", "summary"],
        &["map", "file", "main.rs"],
        &["def", "main.rs:8:4"],
        &["refs", "main.rs:8"],
        &["typedef", "main.rs:8:4"],
        &["implementations", "main.rs:2"],
        &["callers", "main.rs:8"],
        &["callees", "main.rs:1"],
        &["hover", "main.rs:8:4"],
        &["context", "main.rs:8"],
        &["impact", "main.rs:8"],
        &["diff-impact"],
        &["diagnostics", "main.rs"],
        &["actions", "list", "main.rs:8:4"],
        &["rename", "main.rs:8:4", "helper_omega", "--dry-run"],
        &[
            "edit",
            "delete",
            "main.rs",
            "--symbol",
            "helper_alpha",
            "--dry-run",
        ],
        &["doctor", "rust"],
        &["config", "show"],
    ];
    let config = tempfile::tempdir().unwrap();
    std::fs::create_dir(config.path().join("symora")).unwrap();
    std::fs::write(
        config.path().join("symora/config.toml"),
        "[lsp.servers.rust]\ncommand = \"/nonexistent/rust-analyzer-missing\"\n",
    )
    .unwrap();
    let daemon = IsolatedDaemon::for_test("a_read_leaves_a_tree_outside_a_work_tree");
    // One read in a plain directory of its own: what it answered, and whether
    // it left `.symora/` there.
    let read = |through_daemon: bool, args: &[&str]| {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("main.rs"),
            "fn main() {}\nstruct Shape;\nimpl Shape {\n    fn area(&self) -> u32 {\n        \
             0\n    }\n}\nfn helper_alpha() {}\n",
        )
        .unwrap();
        let mut command = match through_daemon {
            true => daemon.command(project.path()),
            false => {
                let mut command = symora();
                command
                    .current_dir(project.path())
                    .env("SYMORA_NO_DAEMON", "1");
                command
            }
        };
        let output = command
            .args(args)
            .args(["--format", "compact"])
            .env("XDG_CONFIG_HOME", config.path())
            .env("GIT_CEILING_DIRECTORIES", std::env::temp_dir())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        let answer = common::parse_response(&output.stdout)
            .unwrap_or_else(|error| panic!("{args:?} answered no JSON: {error}"));
        (answer, project.path().join(".symora").exists())
    };

    // The first read through the daemon starts it, before any other read
    // runs beside it (see `IsolatedDaemon::start`), and the others find it
    // serving rather than race to start one.
    let mut outcomes = vec![(true, READS[0], read(true, READS[0]))];
    let read = &read;
    std::thread::scope(|scope| {
        let in_process: Vec<_> = READS
            .iter()
            .map(|args| (false, *args, scope.spawn(move || read(false, args))))
            .collect();
        let through_daemon: Vec<_> = READS[1..]
            .iter()
            .map(|args| (true, *args, scope.spawn(move || read(true, args))))
            .collect();
        outcomes.extend(
            in_process
                .into_iter()
                .chain(through_daemon)
                .map(|(through_daemon, args, run)| (through_daemon, args, run.join().unwrap())),
        );
    });
    for (through_daemon, args, (answer, wrote)) in outcomes {
        if args.starts_with(&["search", "symbols"]) && args.len() == 3 {
            assert!(
                answer.to_string().contains("only for a git work tree"),
                "{args:?} must have read the store and found no index: {answer}"
            );
        }
        assert!(
            !wrote,
            "{args:?} wrote .symora/ {}: {answer}",
            if through_daemon {
                "through the daemon"
            } else {
                "in process"
            }
        );
    }
    daemon.assert_serving();
}

/// The quiescence gate is only meaningful if it actually recognises the
/// marker — a detector that always answered "clean" would turn the wait
/// into a no-op and put the flaky comparison right back.
#[test]
fn degradation_is_detected_at_any_depth() {
    let top: Value = serde_json::from_str(r#"{"count":0,"indexing":"timed_out"}"#).unwrap();
    let nested: Value =
        serde_json::from_str(r#"{"refs":{"count":0,"indexing":"timed_out"}}"#).unwrap();
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
