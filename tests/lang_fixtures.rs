//! Real-language-server integration tests over committed fixtures.
//!
//! These exercise the full stack — server resolution, venv discovery,
//! workspace configuration, session lifetime — against a real pyright on
//! a committed two-package uv workspace whose every assertion crosses
//! the package boundary. The defect class they guard (server detection,
//! settings delivery, protocol handling) is invisible to hermetic tests.
//!
//! Gated like the daemon parity test: `#[ignore]` by default, opt in with
//! `cargo test --test lang_fixtures -- --ignored`. Setup:
//!   npm install -g pyright
//!   (cd tests/fixtures/python/monorepo && uv sync --frozen)
//! Missing prerequisites skip loudly instead of failing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

const SYMORA: &str = env!("CARGO_BIN_EXE_symora");

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python/monorepo")
}

/// 1-indexed (line, column) of the `occurrence`-th appearance of `ident`,
/// resolved from the fixture source so a fixture edit can't silently
/// invalidate hardcoded positions.
fn find_identifier(path: &Path, ident: &str, occurrence: usize) -> (u32, u32) {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("fixture file {} unreadable: {e}", path.display()));
    let mut seen = 0;
    for (line_idx, line) in content.lines().enumerate() {
        let mut from = 0;
        while let Some(pos) = line[from..].find(ident) {
            let col = from + pos;
            let before_ok = col == 0
                || !line[..col]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after = line[col + ident.len()..].chars().next();
            let after_ok = !after.is_some_and(|c| c.is_alphanumeric() || c == '_');
            if before_ok && after_ok {
                seen += 1;
                if seen == occurrence {
                    return (line_idx as u32 + 1, col as u32 + 1);
                }
            }
            from = col + ident.len();
        }
    }
    panic!(
        "identifier '{ident}' occurrence {occurrence} not found in {}",
        path.display()
    );
}

fn run_in_fixture(args: &[String]) -> Value {
    let output = Command::new(SYMORA)
        .args(args)
        .args(["--format", "compact"])
        .current_dir(fixture_root())
        .env("SYMORA_NO_DAEMON", "1")
        .stderr(Stdio::null())
        .output()
        .expect("run symora");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_else(|| panic!("no output for {args:?}"));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON for {args:?}: {e}"))
}

/// True when the environment can run the test; prints the reason and
/// returns false otherwise — a missing prerequisite is a skip, not a
/// failure (surface "unsupported", don't fake an outcome either way).
fn prerequisites_ready() -> bool {
    let pyright = Command::new("pyright-langserver")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !pyright {
        eprintln!("SKIP: pyright-langserver not on PATH (npm install -g pyright)");
        return false;
    }
    let venv = fixture_root().join(".venv/bin/python");
    if !venv.is_file() {
        eprintln!(
            "SKIP: fixture venv missing — run `uv sync --frozen` in {}",
            fixture_root().display()
        );
        return false;
    }
    true
}

#[test]
#[ignore = "requires pyright + a built fixture venv; run: cargo test --test lang_fixtures -- --ignored"]
fn pyright_resolves_across_package_boundaries() {
    if !prerequisites_ready() {
        return;
    }

    let geometry = fixture_root().join("packages/core/src/fixture_core/geometry.py");
    let report = fixture_root().join("packages/app/src/fixture_app/report.py");
    let geometry_rel = "packages/core/src/fixture_core/geometry.py";
    let report_rel = "packages/app/src/fixture_app/report.py";

    // def on the imported `Circle` in app resolves into core.
    let (import_line, import_col) = find_identifier(&report, "Circle", 1);
    let def = run_in_fixture(&[
        "def".into(),
        format!("{report_rel}:{import_line}:{import_col}"),
    ]);
    assert_eq!(
        def["definition"]["file"], geometry_rel,
        "Circle definition must resolve across the package boundary: {def}"
    );

    // refs on `area`'s definition include the call site in app.
    let (area_line, area_col) = find_identifier(&geometry, "area", 1);
    let refs = run_in_fixture(&[
        "refs".into(),
        format!("{geometry_rel}:{area_line}:{area_col}"),
    ]);
    let files: Vec<&str> = refs["items"]
        .as_array()
        .expect("refs items")
        .iter()
        .filter_map(|i| i["file"].as_str())
        .collect();
    assert!(
        files.contains(&report_rel),
        "area references must include the cross-package call site, got {refs}"
    );

    // diagnostics on the importing file are clean: a broken venv or
    // wiped settings surface as phantom missing-import diagnostics.
    let diagnostics = run_in_fixture(&["diagnostics".into(), report_rel.into()]);
    let import_errors = diagnostics["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|d| {
                    d["message"]
                        .as_str()
                        .is_some_and(|m| m.contains("could not be resolved"))
                })
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        import_errors, 0,
        "cross-package import must resolve via the fixture venv: {diagnostics}"
    );
}
