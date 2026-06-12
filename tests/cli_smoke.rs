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

/// End-to-end proof that `output.max_response_chars` governs the emitted
/// bytes: config load → App → OutputContext → fitted JSON, through the
/// real binary. `search content` falls back to a filesystem scan in an
/// unindexed project, so the test needs no language server.
#[cfg(unix)]
#[test]
fn response_size_ceiling_keeps_json_parseable() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = String::new();
    for i in 0..30 {
        source.push_str(&format!("fn needle_{i:02}() {{ let _ = {i}; }}\n"));
    }
    source.push_str("fn main() {}\n");
    std::fs::write(dir.path().join("main.rs"), source).unwrap();

    let config_dir = dir.path().join(".symora");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[output]\nmax_response_chars = 800\n",
    )
    .unwrap();

    let args = &["--format", "compact", "search", "content", "needle"];
    let out = run_in(dir.path(), args);
    assert!(
        out.status.success(),
        "search content failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("fitted stdout must stay valid JSON");
    assert_eq!(json["truncated"], true);
    assert!(json["showing"].as_u64().unwrap() < json["count"].as_u64().unwrap());
    assert!(
        json["hints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h.as_str().unwrap().contains("max_response_chars")),
        "size-fitted response must disclose the config key; hints: {}",
        json["hints"]
    );
    assert!(
        stdout.trim().chars().count() <= 800,
        "emitted response must fit the ceiling, got {} chars",
        stdout.trim().chars().count()
    );

    // 0 disables the ceiling: the same query emits every match.
    std::fs::write(
        config_dir.join("config.toml"),
        "[output]\nmax_response_chars = 0\n",
    )
    .unwrap();
    let out = run_in(dir.path(), args);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(json.get("truncated").is_none());
    assert_eq!(json["showing"], json["count"]);
    assert!(json["count"].as_u64().unwrap() >= 30);
    assert!(stdout.trim().chars().count() > 800);
    if let Some(hints) = json.get("hints") {
        assert!(
            hints
                .as_array()
                .unwrap()
                .iter()
                .all(|h| !h.as_str().unwrap().contains("max_response_chars"))
        );
    }
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

    // `config show` surfaces the same disclosure under the same field
    // name and presence rule as doctor.
    let out = run_in(dir.path(), &["--format", "compact", "config", "show"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["config"]["lsp"]["timeout_secs"], 99);
    assert_eq!(json["config"]["lsp"]["servers"], serde_json::json!({}));
    let errors = json["config_errors"]
        .as_array()
        .expect("config show must disclose the rejected key");
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().unwrap().contains("lsp.servers.klingon"))
    );

    // A clean config emits no config_errors on config show either.
    std::fs::write(config_dir.join("config.toml"), "[lsp]\ntimeout_secs = 99\n").unwrap();
    let out = run_in(dir.path(), &["--format", "compact", "config", "show"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(json.get("config_errors").is_none());
}

/// An applied edit re-indexes the touched file in the same flow, so a
/// search immediately after the write finds the new content and has no
/// memory of the old — no manual `search index build` in between.
#[cfg(unix)]
#[test]
fn edit_reindexes_the_store_so_search_sees_the_new_content() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lib.rs"), "fn alpha() {}\n").unwrap();

    let out = run_in(dir.path(), &["search", "index", "build"]);
    assert!(
        out.status.success(),
        "index build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `edit replace` resolves no symbols, so this exercises the write +
    // refresh flow without a language server.
    let out = run_in(
        dir.path(),
        &["edit", "replace", "lib.rs:1:1", "--text", "fn beta() {}"],
    );
    assert!(
        out.status.success(),
        "edit replace failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let count_for = |kind: &str, query: &str| -> serde_json::Value {
        let out = run_in(dir.path(), &["--format", "compact", "search", kind, query]);
        assert!(
            out.status.success(),
            "search {kind} '{query}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        json["count"].clone()
    };

    assert_eq!(count_for("symbols", "beta"), 1, "edited symbol not indexed");
    assert_eq!(count_for("symbols", "alpha"), 0, "old symbol still indexed");
    assert_eq!(
        count_for("content", "beta"),
        1,
        "edited content not indexed"
    );
}

/// The C-1 scenario: a built index covering rust plus a rust LSP that
/// cannot start. An empty path-like query consults the index in the same
/// call, so its zero covers rust — it must stay bare (no coverage claim
/// against rust, no `search index build` no-op), exactly like the
/// plain-name query on the same state.
#[cfg(unix)]
#[test]
fn pathlike_zero_with_built_index_stays_bare_for_covered_language() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rs"),
        "fn main() {}\nfn helper_alpha() {}\n",
    )
    .unwrap();
    let config_dir = dir.path().join(".symora");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[lsp.servers.rust]\ncommand = \"/nonexistent/rust-analyzer-missing\"\n",
    )
    .unwrap();

    let out = run_in(dir.path(), &["search", "index", "build"]);
    assert!(
        out.status.success(),
        "index build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for query in ["Nonexistent/missing_xyz", "nonexistent_missing_xyz"] {
        let out = run_in(
            dir.path(),
            &["--format", "compact", "search", "symbols", query],
        );
        assert!(
            out.status.success(),
            "search symbols '{query}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["count"], 0, "query '{query}' should find nothing");
        let hints = json
            .get("hints")
            .and_then(|h| h.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            hints
                .iter()
                .all(|h| !h.as_str().unwrap().contains("does not cover rust")),
            "query '{query}': the index covered rust in this call, \
             yet the zero claims otherwise: {hints:?}"
        );
        let next = json
            .get("next_commands")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            next.iter()
                .all(|c| !c.as_str().unwrap().contains("search index build")),
            "query '{query}': index build is a no-op remedy here: {next:?}"
        );
    }
}
