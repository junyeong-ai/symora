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

/// The JSON of a command that had to succeed.
///
/// Every assertion that a field is ABSENT needs this. An `{"error": ...}`
/// response is missing every other field, so a test that only looks for
/// absence is satisfied just as well by the command failing — and a defect
/// that breaks the command then reads as the property holding.
#[cfg(unix)]
fn json_ok(dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let out = run_in(dir, args);
    assert!(
        out.status.success(),
        "{args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("JSON on stdout")
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
/// A file the embed loop cannot read shortens the corpus on every run, so
/// every run has to say so. The cache's mtime is its freshness record, and
/// stamping the live one on a file that was given up on makes it mean "up to
/// date" instead: the failure would be disclosed once and then never again,
/// leaving every later run — the ones actually likely to be scripted —
/// reading as an exhaustive ranking.
#[cfg(unix)]
#[test]
fn a_permanently_unreadable_file_shortens_every_semantic_run_and_says_so_every_time() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), "pub fn beta() {}\n").unwrap();
    std::fs::set_permissions(root.join("b.rs"), std::fs::Permissions::from_mode(0o000)).unwrap();

    // Whether this run can assert anything is settled by the build's features
    // and by probing the tree — never by reading the output under test, which
    // would take a defect for the environment and pass silently.
    if !cfg!(feature = "embeddings") || std::fs::read_to_string(root.join("b.rs")).is_ok() {
        std::fs::set_permissions(root.join("b.rs"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        return;
    }

    let incomplete_of = || -> serde_json::Value {
        json_ok(
            root,
            &["--format", "compact", "search", "semantic", "alpha"],
        )["incomplete"]
            .clone()
    };

    for run in 1..=3 {
        assert_eq!(
            incomplete_of(),
            serde_json::Value::Bool(true),
            "run {run} did not disclose a file that is still unreadable"
        );
    }

    std::fs::set_permissions(root.join("b.rs"), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        incomplete_of(),
        serde_json::Value::Null,
        "a shortfall that is cured must stop being reported"
    );
}

/// A walk turned away from part of the tree learned nothing about what lives
/// there, so no command may draw a conclusion from its absence. "No languages
/// here", "file not in this project", and "no source files" are all such
/// conclusions, and each has an I/O failure standing behind it that the caller
/// can act on instead.
#[cfg(unix)]
#[test]
fn an_unreadable_subtree_is_reported_as_io_rather_than_as_absence() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let blocked = root.join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("b.rs"), "pub fn beta() {}\n").unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let code_of = |args: &[&str]| -> String {
        let out = run_in(root, args);
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        json["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    // Whether the mode bits constrain this user is probed against the tree,
    // never read off the output under test: a guard that consults the answer
    // would take a defect for the environment and pass silently.
    if std::fs::read_dir(&blocked).is_ok() {
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    // Every language this project has is behind the blocked directory, so
    // "no languages" would be a claim the walk never got to make.
    assert_eq!(code_of(&["--format", "compact", "usage", "beta"]), "io");
    assert_eq!(
        code_of(&["--format", "compact", "symbols", "--name", "beta"]),
        "io"
    );
    assert_eq!(
        code_of(&["--format", "compact", "map", "file", "blocked/b.rs"]),
        "io"
    );
    assert_eq!(
        code_of(&["--format", "compact", "map", "related", "blocked/b.rs"]),
        "io"
    );

    // A path that genuinely is not there still reads as not there.
    std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
    assert_eq!(
        code_of(&["--format", "compact", "map", "file", "nope.rs"]),
        "not_found"
    );

    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every other symbol route confirms a zero against disk — an index miss
/// fans out live or rescans the tree — but no language server honors `*`, so
/// a wildcard has only the index. With no rows there is not even a `backend`
/// field to read, which leaves a bare `count: 0` reading as "no such symbol"
/// when the truth is "written since the last build".
#[cfg(unix)]
#[test]
fn a_wildcard_zero_says_it_was_never_confirmed_against_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), "pub struct AlphaWidget;\n").unwrap();

    let out = run_in(root, &["--format", "compact", "search", "index", "build"]);
    assert!(
        out.status.success(),
        "index build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(root.join("zeta.rs"), "pub struct ZetaWidget;\n").unwrap();

    let out = run_in(
        root,
        &["--format", "compact", "search", "symbols", "*Zeta*"],
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["count"], 0, "the index has no row for it yet");
    let hints = json["hints"]
        .as_array()
        .expect("an unconfirmed zero says so: {json}");
    assert!(
        hints[0]
            .as_str()
            .unwrap()
            .contains("matched against the index alone"),
        "a wildcard zero must not read as absence: {hints:?}"
    );

    // A wildcard that DOES match says nothing extra: every row carries
    // `backend: "index"`, which is the same statement about provenance.
    let out = run_in(
        root,
        &["--format", "compact", "search", "symbols", "*Alpha*"],
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["count"], 1);
    assert!(
        json["hints"].is_null(),
        "a non-empty wildcard answer carries its provenance in `backend`: {json}"
    );
}

/// A build that could not read some paths leaves the index short for
/// languages its scope still names, and every route that answers FROM that
/// index has to say so. The path-like form is the one that regressed: it
/// reaches the same index for the same languages, and a route that decides
/// coverage from `vouched` while deciding the lower bound from something else
/// drops the disclosure for a query that differs only by a `/`.
#[cfg(unix)]
#[test]
fn every_route_that_answers_from_a_holed_index_says_the_count_is_short() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("lib.rs"),
        "pub struct UserStore;\nimpl UserStore {\n    pub fn load(&self) {}\n}\n",
    )
    .unwrap();
    let locked = root.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("hidden.rs"), "pub fn load_hidden() {}\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Probed against the tree, never read off the output under test: a guard
    // that consults the answer would take a defect for the environment and
    // pass silently.
    let mode_bites = std::fs::read_dir(&locked).is_err();
    run_in(root, &["--format", "compact", "search", "index", "build"]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if !mode_bites {
        return;
    }

    for query in ["load", "UserStore/load"] {
        let out = run_in(root, &["--format", "compact", "search", "symbols", query]);
        assert!(
            out.status.success(),
            "search symbols '{query}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            json["incomplete"],
            serde_json::Value::Bool(true),
            "'{query}' answered from a holed index without marking the count a lower bound: {json}"
        );
        // Named for the INDEX specifically. The permissions are restored before
        // the query runs, so this run's own walk reads the tree whole and has
        // no shortfall of its own to stand in for the build's.
        let hints = json["hints"].as_array().expect("a cause names the bound");
        assert!(
            hints
                .iter()
                .any(|hint| hint.as_str().unwrap().contains("The index was built while")),
            "'{query}' set the flag without naming the index as its cause: {hints:?}"
        );
    }
}

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
/// cannot start. The index has no match, so it vouches for nothing — the
/// live recheck it triggers is the answer, and its failure leaves a zero
/// that is not authoritative for rust. The zero says so, and still does
/// not offer `search index build`: the index is already built, so
/// rebuilding it is a no-op remedy.
/// A zero is the one answer read as "nothing exists", so what it rests on has
/// to be checked rather than assumed. With no language server to confirm it,
/// the index is the whole authority: current, and the zero is exact; behind the
/// working tree, and a symbol written since the build has no row to match.
///
/// Both directions are pinned here, and so is the remedy — a disclosure whose
/// fix does not clear it is worse than none, and this one prescribes a rebuild.
#[cfg(unix)]
#[test]
fn a_symbol_zero_is_authoritative_until_the_tree_moves() {
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

    let build = || {
        let out = run_in(dir.path(), &["search", "index", "build"]);
        assert!(
            out.status.success(),
            "index build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let zero = |query: &str| -> serde_json::Value {
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
        json
    };
    let disclosure = |json: &serde_json::Value| -> (Vec<String>, Vec<String>) {
        let read = |key: &str| {
            json.get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .map(|v| v.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        (read("hints"), read("next_commands"))
    };

    build();
    // The build covers rust, so a dead rust server takes nothing out of the
    // answer's domain — naming a gap here would send an agent to install a
    // server the answer never needed.
    for query in ["Nonexistent/missing_xyz", "nonexistent_missing_xyz"] {
        let json = zero(query);
        let (hints, next) = disclosure(&json);
        assert!(
            json.get("coverage_gaps").is_none(),
            "query '{query}': rust is in the build's scope, so its zero is not a coverage gap: {json}"
        );
        assert!(
            hints.is_empty() && next.is_empty(),
            "query '{query}': a current index answers this zero outright: {hints:?} {next:?}"
        );
    }

    // A rename to a name of the same length leaves the file exactly as long as
    // the build found it. That is the shape of the edit these searches are run
    // to check, so it is the one the currency question has to catch — size
    // alone would read this tree as untouched.
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(
        dir.path().join("main.rs"),
        "fn main() {}\nfn helper_omega() {}\n",
    )
    .unwrap();
    let (renamed_hints, _) = disclosure(&zero("helper_omega"));
    assert!(
        renamed_hints
            .iter()
            .any(|h| h.contains("behind the working tree")),
        "a same-size edit moves the tree as surely as any other: {renamed_hints:?}"
    );

    // The tree moves again, this time by growing. The same zero now rests on a
    // build that never saw this declaration, and says so.
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(
        dir.path().join("main.rs"),
        "fn main() {}\nfn helper_alpha() {}\nfn nonexistent_missing_xyz() {}\n",
    )
    .unwrap();

    let (hints, next) = disclosure(&zero("nonexistent_missing_xyz"));
    assert!(
        hints.iter().any(|h| h.contains("behind the working tree")),
        "a zero drawn from an index the tree has outrun must say so: {hints:?}"
    );
    assert!(
        next.iter().any(|c| c == "symora search index build"),
        "the remedy has to be the one that repairs the stated fact: {next:?}"
    );

    // And the remedy clears it: the rebuild sees the new declaration, so the
    // query stops being a zero at all.
    build();
    let out = run_in(
        dir.path(),
        &[
            "--format",
            "compact",
            "search",
            "symbols",
            "nonexistent_missing_xyz",
        ],
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json["count"], 1,
        "the prescribed rebuild must reach the file the disclosure was about: {json}"
    );
}

/// A file that holds no text is outside a search's domain, not a hole in its
/// answer — the line the indexer has always drawn, now drawn once for every
/// surface that reads files. Invalid UTF-8 reaches that verdict through a read
/// error rather than through a NUL byte, and reading it as "could not read"
/// would make an answer that is whole report itself as short.
#[test]
fn a_file_that_is_not_text_is_outside_the_domain_rather_than_a_hole() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
    std::fs::write(root.join("bad.rs"), b"\xff\xfe not utf8 \xc3\x28\n").unwrap();

    for args in [
        &["--format", "compact", "search", "content", "alpha"][..],
        &[
            "--format",
            "compact",
            "search",
            "ast",
            "function_item",
            "-l",
            "rust",
        ][..],
        &["--format", "compact", "pack", "--tokens", "500"][..],
    ] {
        let json = json_ok(root, args);
        assert!(
            json.get("incomplete").is_none(),
            "{args:?} called a non-text file a shortfall: {json}"
        );
    }
}

/// A `--path` the caller named is an assertion about the tree. A typo that
/// searches an empty domain and answers `0` is the worst reading of it, and
/// "could not be read" is not true either — nothing is there to read.
#[test]
fn a_named_path_that_is_not_there_fails_rather_than_answering_zero() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();

    let out = run_in(
        root,
        &[
            "--format",
            "compact",
            "search",
            "ast",
            "function_item",
            "-l",
            "rust",
            "--path",
            "does/not/exist",
        ],
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["error"]["code"], "not_found", "{json}");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does/not/exist"),
        "the path that is missing is the one named: {json}"
    );
}

/// The build's shortfall names the paths it is about, so the reader can act on
/// it. A count alone leaves an agent with `--force` and nowhere to look, and
/// `--force` cannot read a path whose permissions are the problem.
#[cfg(unix)]
#[test]
fn a_holed_build_names_the_paths_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
    let blocked = root.join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("b.rs"), "pub fn beta() {}\n").unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Probed against the tree, never read off the output under test: a guard
    // that consults the answer would take a defect for the environment and
    // pass silently.
    let mode_bites = std::fs::read_dir(&blocked).is_err();
    run_in(root, &["--format", "compact", "search", "index", "build"]);
    let out = run_in(root, &["--format", "compact", "search", "index", "status"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

    if !mode_bites {
        return;
    }
    let paths = json["unread_paths"]
        .as_array()
        .expect("a build that could not read a path names it");
    assert_eq!(
        paths
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["blocked"],
        "the path is named where the reader's own files are: {json}"
    );
}

/// A file the embed loop could not read must leave behind no record a later
/// run can take for freshness. Recording the failure under the mtime the file
/// had is exactly such a record: mtimes recur — a timestamp-preserving
/// restore, or a permission change, which does not move one at all — and the
/// run that meets one skips a file it has never embedded, with nothing left to
/// say the corpus is short. Absence has no value to collide with.
#[cfg(unix)]
#[test]
fn a_read_failure_leaves_no_record_a_later_run_can_take_for_freshness() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
    let target = root.join("b.rs");
    std::fs::write(&target, "pub fn beta() {}\n").unwrap();
    let original = std::fs::metadata(&target).unwrap().modified().unwrap();

    let search = || -> serde_json::Value {
        json_ok(
            root,
            &[
                "--format",
                "compact",
                "search",
                "semantic",
                "database connection pool",
            ],
        )
    };
    // Whether this run can assert anything is settled by the build's features
    // and by probing the tree — never by reading the output under test, which
    // would take a defect for the environment and pass silently.
    if !cfg!(feature = "embeddings") {
        return;
    }
    search();

    // The content moves, so the cache's record of this file is stale and the
    // next run has to read it — which is the run that fails.
    std::fs::write(&target, "pub fn database_connection_pool() {}\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mode_bites = std::fs::read_to_string(&target).is_err();
    search();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    if !mode_bites {
        return;
    }

    // The file comes back with the timestamp it had when it was last embedded
    // successfully — the shape a restore from an archive leaves.
    std::fs::File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(original))
        .unwrap();

    let json = search();
    let items = json["items"].as_array().cloned().unwrap_or_default();
    assert!(
        items
            .iter()
            .any(|item| item["file"].as_str() == Some("b.rs")),
        "the file was never embedded, and a recurring mtime made the run skip it: {json}"
    );
}

/// `stale` speaks for the files behind the items a response actually emitted.
/// The index page it is read from is a superset of them — sorting, the kind
/// filters, and the limit all cut into it — so a page holding one stale row
/// and one fresh one says nothing about an answer that kept only the fresh
/// one, and saying otherwise sends an agent to rebuild over nothing.
#[test]
fn stale_speaks_only_for_the_files_behind_the_items_emitted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.rs"), "pub fn probe_fn() {}\n").unwrap();
    std::fs::write(root.join("s.rs"), "pub struct ProbeStruct;\n").unwrap();
    run_in(root, &["--format", "compact", "search", "index", "build"]);

    // Only the file behind the function moves.
    std::fs::write(root.join("f.rs"), "pub fn probe_fn() { let _x = 1; }\n").unwrap();

    let stale_of = |args: &[&str]| json_ok(root, args)["stale"].clone();
    assert_eq!(
        stale_of(&["--format", "compact", "symbols", "--name", "Probe"]),
        serde_json::Value::Bool(true),
        "the answer holds the row whose file moved"
    );
    assert_eq!(
        stale_of(&[
            "--format", "compact", "symbols", "--name", "Probe", "--kind", "struct"
        ]),
        serde_json::Value::Null,
        "the only row left is backed by a file that never moved"
    );
    assert_eq!(
        stale_of(&[
            "--format", "compact", "symbols", "--name", "Probe", "--kind", "function"
        ]),
        serde_json::Value::Bool(true),
        "and the row that did move still says so on its own"
    );
}

/// The tier a run answers from is an input, not an outcome. Without the flag
/// `symbols` prefers a language server, so the same file reads differently on
/// a machine that has one; with it the answer comes from the grammar and says
/// so, which is what lets a caller gate on it. A command with no such source
/// refuses instead of degrading into an answer wearing the same shape.
#[test]
fn a_confined_run_answers_from_the_grammar_or_refuses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rs"),
        "pub struct Widget;
impl Widget {
    pub fn assemble(&self) {}
}
",
    )
    .unwrap();

    let confined = json_ok(dir.path(), &["symbols", "main.rs", "--deterministic"]);
    assert_eq!(
        confined["backend"], "ast",
        "a confined run must read the grammar, never a server: {confined}"
    );
    let names: Vec<&str> = confined["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        names.contains(&"assemble"),
        "the grammar answer is still a real one: {names:?}"
    );

    let out = run_in(dir.path(), &["refs", "main.rs:3:12", "--deterministic"]);
    let refused: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    assert_eq!(
        refused["error"]["code"], "unsupported",
        "references have no source that derives from the tree alone: {refused}"
    );
}

/// A mutation addresses a declaration through the same reader every other
/// surface does, so a file the grammar can read is editable whether or not a
/// language server is installed. Resolving it through the server alone made
/// `edit` refuse the languages `symbols` had just answered for.
#[test]
fn a_declaration_the_grammar_reads_is_editable_without_a_server() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.tf"),
        "terraform {\n  required_version = \">= 1.0\"\n}\n\nvariable \"project_id\" {\n  type = string\n}\n",
    )
    .unwrap();
    let config_dir = dir.path().join(".symora");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[lsp.servers.terraform]\ncommand = \"/nonexistent/terraform-ls\"\n",
    )
    .unwrap();

    json_ok(
        dir.path(),
        &[
            "edit",
            "replace-body",
            "main.tf",
            "--symbol",
            "project_id",
            "--body",
            "variable \"project_id\" {\n  type    = string\n  default = \"aix\"\n}",
        ],
    );

    let after = std::fs::read_to_string(dir.path().join("main.tf")).unwrap();
    assert!(
        after.contains("default = \"aix\""),
        "the edit did not land: {after}"
    );
    assert!(
        after.contains("required_version"),
        "the edit took a neighbouring block with it: {after}"
    );
}

/// A symbol-path answer merges the index with a live lookup, so each row says
/// which one produced it — the word `search symbols` already uses. Without it
/// a caller cannot tell a row the index vouches for from one a language server
/// supplied, which is the difference between a checkable answer and a guess.
#[test]
fn a_merged_symbol_answer_names_each_row_producer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mod.py"),
        "class Holder:\n    def existing(self):\n        pass\n",
    )
    .unwrap();
    json_ok(dir.path(), &["search", "index", "build"]);

    let page = json_ok(
        dir.path(),
        &["symbols", "--symbol", "Holder/existing", "--lang", "python"],
    );
    let rows = page["items"].as_array().expect("items");
    assert!(!rows.is_empty(), "the index holds this symbol: {page}");
    for row in rows {
        assert!(
            row["backend"].is_string(),
            "a merged answer must name each row's producer: {page}"
        );
    }
    assert_eq!(rows[0]["backend"], "index");

    let confined = json_ok(
        dir.path(),
        &[
            "symbols",
            "--symbol",
            "Holder/existing",
            "--lang",
            "python",
            "--deterministic",
        ],
    );
    for row in confined["items"].as_array().expect("items") {
        assert_eq!(
            row["backend"], "index",
            "a confined answer holds no row a server supplied: {confined}"
        );
    }
}

/// Ranking orders an answer; it does not decide which declarations a caller
/// is allowed to see. A query that matches both callables and named values
/// returns all of them up to the limit asked for, ranked — dropping the rest
/// would report `truncated`, whose remedy is a larger limit, for rows no
/// limit brings back.
#[test]
fn a_symbol_search_emits_every_match_the_limit_admits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mod.py"),
        "def alpha_one():\n    pass\n\n\ndef alpha_two():\n    pass\n\n\ndef alpha_three():\n    pass\n\n\nALPHA_LEFT = 1\nALPHA_RIGHT = 2\n",
    )
    .unwrap();
    json_ok(dir.path(), &["search", "index", "build"]);

    let page = json_ok(
        dir.path(),
        &[
            "search",
            "symbols",
            "alpha",
            "--lang",
            "python",
            "--limit",
            "10",
            "--deterministic",
        ],
    );
    let rows = page["items"].as_array().expect("items");
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(
        rows.len(),
        5,
        "three callables and two named values all match under the limit: {page}"
    );
    assert!(
        names.contains(&"ALPHA_LEFT") && names.contains(&"ALPHA_RIGHT"),
        "a named value is a declaration, not noise to drop: {names:?}"
    );
    assert_eq!(
        page["truncated"].as_bool(),
        None,
        "nothing was held back, so nothing claims it was: {page}"
    );
}

/// `diff-impact` stops at `--max-symbols` before it runs out of changed
/// symbols, and every count it publishes is then a lower bound. Saying so is
/// what lets a reviewer treat it as a gap to close rather than a population to
/// reason from — the counts alone read as the whole blast radius.
#[test]
fn a_capped_diff_analysis_says_its_counts_are_a_lower_bound() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.py"), "def one():\n    pass\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "init"]);
    std::fs::write(
        repo.join("a.py"),
        "def one():\n    return 1\n\n\ndef two():\n    pass\n\n\ndef three():\n    pass\n",
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "change"]);

    let capped = json_ok(repo, &["diff-impact", "HEAD~1", "--max-symbols", "1"]);
    assert_eq!(
        capped["incomplete"].as_bool(),
        Some(true),
        "a capped analysis publishes counts that are short: {capped}"
    );
    assert!(
        capped["hints"].as_array().is_some_and(|h| h
            .iter()
            .any(|x| x.as_str().is_some_and(|s| s.contains("--max-symbols")))),
        "the remedy names the cap that stopped it: {capped}"
    );

    let whole = json_ok(repo, &["diff-impact", "HEAD~1", "--max-symbols", "0"]);
    assert_eq!(
        whole["incomplete"].as_bool(),
        None,
        "an analysis that ran out of candidates claims no shortfall: {whole}"
    );
}

/// A diff's changed symbols are read through the same reader every other
/// surface uses, so a language with no server still reports WHAT changed.
/// Reference counts stay absent and say so — a count no server can supply is
/// never fabricated, and a file the grammar can read is never called
/// unmeasured.
#[test]
fn a_diff_in_a_serverless_language_still_names_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "t"]);
    std::fs::write(
        repo.join("main.tf"),
        "variable \"project_id\" {\n  type = string\n}\n",
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "init"]);
    std::fs::write(
        repo.join("main.tf"),
        "variable \"project_id\" {\n  type    = string\n  default = \"aix\"\n}\n\noutput \"bucket_url\" {\n  value = 1\n}\n",
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "change"]);

    let page = json_ok(repo, &["diff-impact", "HEAD~1"]);
    let names: Vec<&str> = page["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(
        names.contains(&"project_id"),
        "the grammar reads this file, so the diff names what it changed: {page}"
    );
    assert!(
        page["unmeasured_files"].is_null(),
        "a file the grammar read is not unmeasured: {page}"
    );
    for change in page["changes"].as_array().expect("changes") {
        assert!(
            change["refs"].is_null(),
            "no server can count references here, so none are claimed: {page}"
        );
    }
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An answer is ranked before it is cut, so a small limit returns the best
/// matches rather than whichever ones the index happened to page in. The
/// index orders its page by textual relevance alone; the ranking that orders
/// the answer also demotes test files, and below a cut a demotion is a
/// removal — so a page selected without the ranking hands back rows the
/// ranking would have put last.
#[test]
fn a_short_answer_holds_the_best_matches_not_the_first_page() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("tests")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    // Bare `handler` at the top of each test file: the same textual relevance
    // as the production method and a shorter symbol path, so the index pages
    // every one of them in ahead of it.
    for n in 0..6 {
        std::fs::write(
            dir.path().join(format!("tests/test_{n}.py")),
            "def handler():\n    pass\n",
        )
        .unwrap();
    }
    std::fs::write(
        dir.path().join("src/api.py"),
        "class RequestDispatcher:\n    def handler(self):\n        pass\n",
    )
    .unwrap();
    json_ok(dir.path(), &["search", "index", "build"]);

    let short = json_ok(
        dir.path(),
        &[
            "search",
            "symbols",
            "handler",
            "--limit",
            "2",
            "--deterministic",
        ],
    );
    let files: Vec<&str> = short["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|i| i["file"].as_str())
        .collect();
    assert_eq!(files.len(), 2, "the limit admits two rows: {short}");
    assert!(
        files[0].starts_with("src/"),
        "the index pages the shorter test paths in first; the ranking leads with the production \
         declaration: {short}"
    );

    let whole = json_ok(
        dir.path(),
        &[
            "search",
            "symbols",
            "handler",
            "--limit",
            "50",
            "--deterministic",
        ],
    );
    let head: Vec<&str> = whole["items"]
        .as_array()
        .expect("items")
        .iter()
        .take(2)
        .filter_map(|i| i["file"].as_str())
        .collect();
    assert_eq!(
        files, head,
        "a limit bounds the answer; it does not change which matches lead it: {whole}"
    );
}

/// The two symbol surfaces answer one question, so they order it one way. Two
/// copies of the ranking had already drifted apart on which kinds are
/// low-signal, which put the same symbol at the head of one answer and the
/// tail of the other.
#[test]
fn both_symbol_surfaces_order_a_query_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("tests")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("tests/test_store.py"),
        "def store():\n    pass\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/store.py"),
        "class StoreService:\n    def store(self):\n        pass\n",
    )
    .unwrap();
    json_ok(dir.path(), &["search", "index", "build"]);

    let searched = json_ok(
        dir.path(),
        &[
            "search",
            "symbols",
            "store",
            "--lang",
            "python",
            "--deterministic",
        ],
    );
    let named = json_ok(
        dir.path(),
        &[
            "symbols",
            "--name",
            "store",
            "--lang",
            "python",
            "--deterministic",
        ],
    );

    let lead = |v: &serde_json::Value, file_at: fn(&serde_json::Value) -> Option<&str>| {
        v["items"]
            .as_array()
            .expect("items")
            .first()
            .and_then(file_at)
            .map(str::to_string)
    };
    let searched_lead = lead(&searched, |i| i["file"].as_str());
    let named_lead = lead(&named, |i| i["location"]["file"].as_str());
    assert!(searched_lead.is_some(), "search found nothing: {searched}");
    assert_eq!(
        searched_lead, named_lead,
        "one question, two orders: {searched} vs {named}"
    );
}
