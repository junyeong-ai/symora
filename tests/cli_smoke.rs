//! End-to-end smoke tests for command-level wiring.
//!
//! These tests run the released binary against the Symora repo itself —
//! every subcommand must parse, every `--help` must print, and the
//! LSP-less commands (`map`, `pack`) must emit valid JSON / markdown.
//! They give us a fast regression net for clap dispatch, the MCP stdio
//! loop, and the JSON contract without requiring a real LSP server.

use std::io::Write;
use std::process::{Command, Stdio};

const SYMORA: &str = env!("CARGO_BIN_EXE_symora");

const ALL_SUBCOMMANDS: &[&str] = &[
    "init",
    "status",
    "config",
    "doctor",
    "symbols",
    "def",
    "refs",
    "typedef",
    "implementations",
    "callers",
    "callees",
    "supertypes",
    "subtypes",
    "hover",
    "signature",
    "context",
    "impact",
    "diff-impact",
    "usage",
    "diagnostics",
    "search",
    "map",
    "pack",
    "bench",
    "edit",
    "rename",
    "actions",
    "inlay-hints",
    "folding",
    "selection",
    "code-lens",
    "format",
    "daemon",
    "mcp",
];

fn run(args: &[&str]) -> std::process::Output {
    Command::new(SYMORA)
        .args(args)
        .env("SYMORA_NO_DAEMON", "1")
        .output()
        .expect("symora binary should exist")
}

#[cfg(unix)]
fn run_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(SYMORA)
        .args(args)
        .env("SYMORA_NO_DAEMON", "1")
        .current_dir(dir)
        .output()
        .expect("symora binary should exist")
}

#[test]
fn root_help_lists_every_subcommand() {
    let out = run(&["--help"]);
    assert!(
        out.status.success(),
        "root --help failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for cmd in ALL_SUBCOMMANDS {
        assert!(stdout.contains(cmd), "root help missing subcommand {cmd}");
    }
}

#[test]
fn every_subcommand_help_succeeds() {
    for sub in ALL_SUBCOMMANDS {
        let out = run(&[sub, "--help"]);
        assert!(
            out.status.success(),
            "{sub} --help failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn map_summary_emits_valid_json_with_compact_format() {
    let out = run(&["--format", "compact", "map", "summary"]);
    assert!(
        out.status.success(),
        "map summary failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("map summary stdout must be valid JSON");
    assert!(json.get("languages").is_some());
    assert!(
        json["total_files"].as_u64().unwrap_or(0) > 0,
        "map summary should discover files in this repo"
    );
}

#[test]
fn pack_emits_valid_json_with_focus_bias() {
    let out = run(&[
        "--format", "compact", "pack", "--tokens", "500", "--focus", "src",
    ]);
    assert!(out.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("pack stdout must be valid JSON");
    assert_eq!(json["focus"], "src");
    assert!(json["files"]["count"].as_u64().unwrap_or(0) > 0);
    assert!(json["budget_tokens"].as_u64().unwrap() == 500);
}

#[test]
fn pack_markdown_shape_emits_plain_text_header() {
    let out = run(&["pack", "--tokens", "500", "--shape", "markdown"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("# Symora context pack"));
}

#[test]
fn unknown_subcommand_is_rejected_with_nonzero_exit() {
    let out = run(&["definitelynotacommand"]);
    assert!(!out.status.success());
}

#[test]
fn token_estimate_writes_to_stderr_only() {
    let out = run(&["--format", "compact", "--token-estimate", "map", "summary"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // stdout stays a single JSON line — token-estimate must not pollute it.
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .expect("stdout stays valid JSON when token-estimate is on");
    assert!(stderr.contains("token-estimate"));
}

#[test]
fn mcp_initialize_returns_protocol_version() {
    let mut child = Command::new(SYMORA)
        .args(["mcp", "serve"])
        .env("SYMORA_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn symora mcp serve");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().expect("at least one response line");
    let json: serde_json::Value = serde_json::from_str(line).expect("valid JSON-RPC response");
    assert_eq!(
        json["result"]["protocolVersion"],
        symora::mcp::MCP_PROTOCOL_VERSION
    );
    assert_eq!(json["result"]["serverInfo"]["name"], "symora");
}

#[test]
fn mcp_tools_list_advertises_pack_and_search() {
    let mut child = Command::new(SYMORA)
        .args(["mcp", "serve"])
        .env("SYMORA_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn symora mcp serve");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
        )
        .unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let tools_line = stdout.lines().nth(1).expect("two response lines");
    let json: serde_json::Value = serde_json::from_str(tools_line).expect("valid JSON-RPC");
    let names: Vec<&str> = json["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for required in [
        "build_context_pack",
        "search_symbols",
        "get_impact",
        "rename_symbol",
        "apply_code_action",
        "replace_symbol_body",
    ] {
        assert!(
            names.contains(&required),
            "tools/list missing {required}; got {names:?}"
        );
    }
}

#[test]
fn pretty_format_produces_multiline_output() {
    let out = run(&["--format", "pretty", "map", "summary"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Pretty JSON is multi-line; compact is single-line.
    assert!(stdout.lines().count() > 5);
}

#[test]
fn quiet_mode_suppresses_stdout_for_success() {
    let out = run(&["--quiet", "map", "summary"]);
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "quiet mode should produce no stdout, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[cfg(unix)]
#[test]
fn doctor_reports_override_provenance_and_spawnability() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fake-rust-analyzer");
    std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let config_dir = dir.path().join(".symora");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!("[lsp.servers.rust]\ncommand = \"{}\"\n", bin.display()),
    )
    .unwrap();

    // The override applies: rust resolves to the configured binary and the
    // row discloses provenance.
    let out = run_in(dir.path(), &["--format", "compact", "doctor", "rust"]);
    assert!(out.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor stdout must be valid JSON");
    let rust = json["languages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "rust")
        .expect("doctor must report a rust row");
    assert_eq!(rust["installed"], true);
    assert_eq!(rust["source"], "config");
    assert_eq!(rust["command"], bin.to_str().unwrap());

    // Builtin rows carry neither source nor command, and a clean config
    // emits no config_errors at all.
    let out = run_in(dir.path(), &["--format", "compact", "doctor"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let go = json["languages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "go")
        .expect("doctor must report a go row");
    assert!(go.get("source").is_none());
    assert!(go.get("command").is_none());
    assert!(json.get("config_errors").is_none());

    // A rejected key is disclosed in config_errors, never applied, and
    // never costs the rest of the config.
    std::fs::write(
        config_dir.join("config.toml"),
        "[lsp]\ntimeout_secs = 99\n\n[lsp.servers.klingon]\ncommand = \"/nope\"\n",
    )
    .unwrap();
    let out = run_in(dir.path(), &["--format", "compact", "doctor", "rust"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let errors = json["config_errors"]
        .as_array()
        .expect("config_errors must be present for a rejected key");
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().unwrap().contains("lsp.servers.klingon"))
    );
    let rust = json["languages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "rust")
        .unwrap();
    assert!(rust.get("source").is_none());

    let out = run_in(dir.path(), &["--format", "compact", "config", "show"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["config"]["lsp"]["timeout_secs"], 99);
    assert_eq!(json["config"]["lsp"]["servers"], serde_json::json!({}));
}
