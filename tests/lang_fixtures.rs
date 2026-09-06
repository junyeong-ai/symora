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
        .expect("diagnostics items")
        .iter()
        .filter(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("could not be resolved"))
        })
        .count();
    assert_eq!(
        import_errors, 0,
        "cross-package import must resolve via the fixture venv: {diagnostics}"
    );
}

/// `context --with-bodies` attaches a complete verbatim body —
/// whole-body-or-nothing — and a starved budget discloses zero
/// attachments without touching the items themselves. Exercised through
/// the types section because pyright does not serve outgoing calls; both
/// sections share one resolution and budget path, and the type here
/// crosses the package boundary.
#[test]
#[ignore = "requires pyright + a built fixture venv; run: cargo test --test lang_fixtures -- --ignored"]
fn context_with_bodies_attaches_whole_type_bodies() {
    if !prerequisites_ready() {
        return;
    }

    let report = fixture_root().join("packages/app/src/fixture_app/report.py");
    let report_rel = "packages/app/src/fixture_app/report.py";
    let geometry = fixture_root().join("packages/core/src/fixture_core/geometry.py");

    let (line, col) = find_identifier(&report, "circle", 1);
    let context = run_in_fixture(&[
        "context".into(),
        format!("{report_rel}:{line}:{col}"),
        "--types".into(),
        "--with-bodies".into(),
    ]);

    let types = &context["types"];
    let items = types["items"].as_array().expect("types items");
    let circle_body = items
        .iter()
        .find(|i| i["name"].as_str() == Some("Circle"))
        .and_then(|i| i["body"].as_str())
        .unwrap_or_else(|| panic!("Circle type must carry a body: {context}"));

    // Verbatim completeness: the body is a contiguous slice of the real
    // file, spanning the class head through its last method — never a
    // partial or reconstructed fragment.
    let geometry_src = std::fs::read_to_string(&geometry).expect("fixture source");
    assert!(
        geometry_src.contains(circle_body),
        "body must be a verbatim slice of {}: {circle_body:?}",
        geometry.display()
    );
    assert!(circle_body.starts_with("class Circle:"));
    assert!(circle_body.contains("return 2 * PI * self.radius"));
    assert_eq!(
        types["bodies_included"].as_u64().expect("bodies_included") as usize,
        items.iter().filter(|i| i.get("body").is_some()).count(),
        "bodies_included must equal the items carrying a body: {context}"
    );

    // A starved budget admits nothing — whole-body-or-nothing, disclosed
    // as zero, with the items themselves unchanged.
    let starved = run_in_fixture(&[
        "context".into(),
        format!("{report_rel}:{line}:{col}"),
        "--types".into(),
        "--with-bodies".into(),
        "--body-tokens".into(),
        "1".into(),
    ]);
    assert_eq!(
        starved["types"]["bodies_included"], 0,
        "a 1-token budget must disclose zero attachments: {starved}"
    );
    let stripped: Vec<Value> = items
        .iter()
        .map(|i| {
            let mut item = i.clone();
            item.as_object_mut().unwrap().remove("body");
            item
        })
        .collect();
    assert_eq!(
        starved["types"]["items"].as_array().expect("items"),
        &stripped,
        "starving the budget must not change the items themselves"
    );
}

/// An empty reference set means different things for a member and for a free
/// item. A call reaches a member through the type that declares it —
/// construction, dispatch, a protocol satisfied structurally — and none of
/// those name the member, so its zero is not reachability. `Circle.__init__`
/// is the ordinary case: the fixture builds a `Circle`, which names the class
/// and never the constructor. A free item has no such route, so its zero
/// stands bare rather than being qualified into uselessness.
#[test]
#[ignore = "requires pyright + a built fixture venv; run: cargo test --test lang_fixtures -- --ignored"]
fn an_empty_reference_set_says_what_kind_of_empty_it_is() {
    if !prerequisites_ready() {
        return;
    }
    let geometry = fixture_root().join("packages/core/src/fixture_core/geometry.py");
    let at = |ident: &str, occurrence: usize| {
        let (line, column) = find_identifier(&geometry, ident, occurrence);
        run_in_fixture(&[
            "refs".to_string(),
            format!("packages/core/src/fixture_core/geometry.py:{line}:{column}"),
        ])
    };

    let constructor = at("__init__", 1);
    assert_eq!(
        constructor["count"], 0,
        "the fixture never names the constructor: {constructor}"
    );
    let hints = constructor["hints"].as_array().expect("hints");
    assert!(
        hints.iter().any(|h| {
            h.as_str()
                .is_some_and(|h| h.contains("Circle") && h.contains("not evidence"))
        }),
        "a member of a referenced type must say its zero is not reachability: {constructor}"
    );

    let free_function = at("area", 1);
    assert!(
        free_function["count"].as_u64().is_some_and(|n| n > 0),
        "the fixture calls `area` across the package boundary: {free_function}"
    );

    let class = at("Circle", 1);
    assert!(
        class["hints"].as_array().is_none_or(|h| h
            .iter()
            .all(|x| !x.as_str().is_some_and(|s| s.contains("not evidence")))),
        "a type that IS referenced needs no such qualifier: {class}"
    );

    // `impact` publishes a `risk` verdict derived from the same count, so it
    // is the surface where an unqualified zero does the most damage.
    let (line, column) = find_identifier(&geometry, "__init__", 1);
    let loc = format!("packages/core/src/fixture_core/geometry.py:{line}:{column}");
    for command in ["impact", "context"] {
        let page = run_in_fixture(&[command.to_string(), loc.clone()]);
        assert_eq!(page["refs"]["total"], 0, "{command}: {page}");
        assert!(
            page["hints"].as_array().is_some_and(|h| h
                .iter()
                .any(|x| x.as_str().is_some_and(|s| s.contains("Circle")))),
            "{command} publishes the same zero and must qualify it the same way: {page}"
        );
    }
}
