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
//!   (cd tests/fixtures/python/monorepo && npm ci --ignore-scripts && uv sync --frozen)
//!   PATH="$PWD/tests/fixtures/python/monorepo/node_modules/.bin:$PATH" \
//!     cargo test --test lang_fixtures -- --include-ignored
//! Missing prerequisites skip loudly instead of failing, except under `CI`,
//! where the job installs them and a skip would pass without testing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

mod common;

use common::symora;

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
    let output = symora()
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
    if !pyright_on_path() {
        return false;
    }
    let venv = fixture_root().join(".venv/bin/python");
    if !venv.is_file() {
        return skip(&format!(
            "fixture venv missing — run `uv sync --frozen` in {}",
            fixture_root().display()
        ));
    }
    true
}

/// Report a missing prerequisite: a skip locally, a failure under `CI`.
fn skip(reason: &str) -> bool {
    assert!(
        std::env::var_os("CI").is_none(),
        "CI installs every prerequisite, so none may be missing: {reason}"
    );
    eprintln!("SKIP: {reason}");
    false
}

fn pyright_on_path() -> bool {
    // `pyright-langserver` exits non-zero on any flag it is not serving
    // with; the package's `pyright` answers `--version` only when it runs.
    let found = Command::new("pyright")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    found || skip("pyright does not run here — follow tests/fixtures/python/monorepo/README.md")
}

#[test]
#[ignore = "requires pyright; run: cargo test --test lang_fixtures -- --ignored"]
fn explicit_settings_resolve_a_package_without_a_virtual_environment() {
    if !pyright_on_path() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let core = "packages/core/src/fixture_core/geometry.py";
    let app = "packages/app/src/fixture_app/report.py";
    for path in [core, app] {
        let destination = root.path().join(path);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(fixture_root().join(path), destination).unwrap();
    }
    std::fs::create_dir(root.path().join(".symora")).unwrap();
    assert!(!root.path().join(".venv").exists());
    let (line, column) = find_identifier(&root.path().join(app), "Circle", 1);
    let run = || {
        let output = symora()
            .args([
                "def",
                &format!("{app}:{line}:{column}"),
                "--format",
                "compact",
            ])
            .current_dir(root.path())
            .env("SYMORA_NO_DAEMON", "1")
            .env_remove("VIRTUAL_ENV")
            .env_remove("PYTHONPATH")
            .output()
            .unwrap();
        let response: Value = common::parse_response(&output.stdout).unwrap();
        assert!(output.status.success(), "{response}");
        assert!(response.get("error").is_none(), "{response}");
        response
    };
    let missing = run();
    assert_ne!(missing["definition"]["file"], core, "{missing}");
    std::fs::write(
        root.path().join(".symora/config.toml"),
        "[lsp.servers.python.settings.python.analysis]\nextraPaths = [\"packages/core/src\"]\n",
    )
    .unwrap();
    let configured = run();
    assert_eq!(configured["definition"]["file"], core, "{configured}");
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

    let signed = run_in_fixture(&[
        "symbols".into(),
        geometry_rel.into(),
        "--symbol".into(),
        "Circle".into(),
        "--signature".into(),
        "--depth".into(),
        "3".into(),
    ]);
    assert_eq!(signed["backend"], "document", "{signed}");
    let items = signed["items"].as_array().expect("signature items");
    let circle = items
        .iter()
        .find(|item| item["name"] == "Circle")
        .expect("Circle signature");
    let perimeter = circle["children"]
        .as_array()
        .expect("Circle members")
        .iter()
        .find(|item| item["name"] == "perimeter")
        .expect("perimeter method");
    assert!(
        perimeter["signature"]
            .as_str()
            .is_some_and(|signature| signature.contains("perimeter(self) -> float")),
        "nested method signature is missing: {signed}"
    );
    fn has_body(value: &Value) -> bool {
        match value {
            Value::Object(object) => object.contains_key("body") || object.values().any(has_body),
            Value::Array(items) => items.iter().any(has_body),
            _ => false,
        }
    }
    assert!(
        !has_body(&signed),
        "signature output contains source bodies: {signed}"
    );

    // A nested tree lists each declaration once: a method is under its class,
    // not beside it as well.
    let tree = run_in_fixture(&[
        "symbols".into(),
        geometry_rel.into(),
        "--depth".into(),
        "1".into(),
    ]);
    assert_eq!(tree["backend"], "document", "{tree}");
    fn declarations(items: &Value, out: &mut Vec<String>) {
        for item in items.as_array().into_iter().flatten() {
            out.push(item["name_path"].as_str().unwrap_or_default().to_string());
            declarations(&item["children"], out);
        }
    }
    let mut listed = Vec::new();
    declarations(&tree["items"], &mut listed);
    let distinct: std::collections::HashSet<_> = listed.iter().collect();
    assert!(
        listed.iter().any(|path| path == "Circle/perimeter"),
        "{tree}"
    );
    assert_eq!(distinct.len(), listed.len(), "{tree}");
    assert_eq!(
        tree["count"],
        tree["items"].as_array().unwrap().len(),
        "{tree}"
    );

    // Counting references takes each listed declaration and a class's members,
    // never what a function holds.
    let counted = run_in_fixture(&[
        "symbols".into(),
        geometry_rel.into(),
        "--depth".into(),
        "2".into(),
        "--refs".into(),
    ]);
    // Each row with whether it is internal to something it is inside: what a
    // type or module holds are its members, what anything else holds is not.
    fn flatten<'a>(items: &'a Value, internal: bool, out: &mut Vec<(&'a Value, bool)>) {
        for item in items.as_array().into_iter().flatten() {
            out.push((item, internal));
            let holds_members = matches!(
                item["kind"].as_str(),
                Some(
                    "file"
                        | "module"
                        | "namespace"
                        | "package"
                        | "class"
                        | "interface"
                        | "struct"
                        | "enum"
                        | "object"
                )
            );
            flatten(&item["children"], internal || !holds_members, out);
        }
    }
    let mut rows = Vec::new();
    flatten(&counted["items"], false, &mut rows);
    let row = |path: &str| {
        rows.iter()
            .map(|(row, _)| row)
            .find(|row| row["name_path"] == path)
            .unwrap_or_else(|| panic!("{path} is listed: {counted}"))
    };
    assert!(row("area")["prod_refs"].as_u64() >= Some(1), "{counted}");
    assert!(
        row("Circle/perimeter")["refs"].as_u64() >= Some(1),
        "{counted}"
    );
    assert!(rows.iter().any(|(_, internal)| *internal), "{counted}");
    for (row, internal) in &rows {
        let measured = row.get("refs").is_some() || row.get("refs_status").is_some();
        assert_eq!(measured, !internal, "{row}");
    }

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
    // as zero, preserving identity and naming the omission reason.
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
            item["body_omitted"] = Value::String("token_budget".to_string());
            item
        })
        .collect();
    assert_eq!(
        starved["types"]["items"].as_array().expect("items"),
        &stripped,
        "starving the budget preserves identity and discloses omission"
    );

    let (target_line, target_column) = find_identifier(&report, "describe", 1);
    let target = run_in_fixture(&[
        "context".into(),
        format!("{report_rel}:{target_line}:{target_column}"),
        "--body".into(),
        "--body-tokens".into(),
        "1".into(),
    ]);
    assert_eq!(target["target"]["name"], "describe");
    assert_eq!(target["target"]["body_omitted"], "token_budget");
    assert!(target["target"].get("body").is_none());
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

/// pyright lists a function's parameters and locals among its symbols. A
/// changed line among them belongs to the function, which is what callers
/// and tests name and what `diff-impact` counts references for; a function
/// renamed with its parameters is the old name deleted and the new one
/// changed.
#[test]
#[ignore = "requires pyright; run: cargo test --test lang_fixtures -- --ignored"]
fn diff_impact_hands_a_changed_local_to_its_function() {
    if !pyright_on_path() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdout(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(
        repo.join("m.py"),
        "def compute(a):\n    total = a + 1\n    return total\n\n\n\
         def old_name(a):\n    return a\n\n\n\
         def use():\n    return compute(1) + old_name(2)\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "one"]);
    std::fs::write(
        repo.join("m.py"),
        "def compute(a, b=0):\n    total = a + 2\n    return total\n\n\n\
         def new_name(a):\n    return a\n\n\n\
         def use():\n    return compute(1) + old_name(2)\n",
    )
    .unwrap();

    let output = symora()
        .args(["diff-impact", "--format", "compact"])
        .current_dir(repo)
        .env("SYMORA_NO_DAEMON", "1")
        .stderr(Stdio::null())
        .output()
        .expect("run symora");
    let page: Value = common::parse_response(&output.stdout).expect("JSON on stdout");
    let rows: Vec<String> = page["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|c| format!("{} {}", c["name"], c["change_type"]))
        .collect();
    assert_eq!(
        rows,
        [
            r#""old_name" "deleted""#,
            r#""compute" "modified""#,
            r#""new_name" "modified""#
        ],
        "{page}"
    );
    assert!(
        page["changes"][1]["refs"].is_u64(),
        "pyright answered, so the function's references are counted: {page}"
    );
}

/// A class's lines that none of its members covers — its decorator, its
/// bases, a comment between its members — are the class's: a change to any
/// one of them modifies the class, as pyright's range for it says.
#[test]
#[ignore = "requires pyright; run: cargo test --test lang_fixtures -- --ignored"]
fn diff_impact_gives_a_class_the_lines_no_member_covers() {
    if !pyright_on_path() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdout(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    let service = |decorator: &str, bases: &str, between: &str| {
        format!(
            "import dataclasses\n\n\nclass Base:\n    pass\n\n\nclass Mixin:\n    pass\n\n\n\
             @dataclasses.dataclass{decorator}\nclass Service(\n{bases}):\n    \
             name: str = \"\"\n{between}\n    def run(self):\n        return self.name\n"
        )
    };
    std::fs::write(repo.join("svc.py"), service("", "    Base,\n", "")).unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "one"]);

    for (change, content) in [
        ("decorator", service("(frozen=True)", "    Base,\n", "")),
        ("bases", service("", "    Base,\n    Mixin,\n", "")),
        (
            "between members",
            service("", "    Base,\n", "    # runs it\n"),
        ),
    ] {
        std::fs::write(repo.join("svc.py"), content).unwrap();
        let output = symora()
            .args(["diff-impact", "--format", "compact"])
            .current_dir(repo)
            .env("SYMORA_NO_DAEMON", "1")
            .stderr(Stdio::null())
            .output()
            .expect("run symora");
        let page: Value = common::parse_response(&output.stdout).expect("JSON on stdout");
        let rows: Vec<String> = page["changes"]
            .as_array()
            .expect("changes")
            .iter()
            .map(|c| format!("{} {}", c["name"], c["change_type"]))
            .collect();
        assert_eq!(rows, [r#""Service" "modified""#], "{change}: {page}");
        assert!(
            page["changes"][0]["refs"].is_u64(),
            "{change}: pyright answered, so the class's references are counted: {page}"
        );
    }
}

/// pyright gives a variable the range of its name alone, which a whole-line
/// edit could only split; the declaration the grammar reads at that name
/// gives the answer the whole assignment, as it does wherever a server's
/// range leaves out part of a declaration. An assignment that binds several
/// names is the whole declaration of each, so a declaration inserted after
/// one goes after all of it, and deleting one is refused rather than taking
/// the others, as is deleting a name an assignment binds along with an
/// attribute.
#[test]
#[ignore = "requires pyright; run: cargo test --test lang_fixtures -- --ignored"]
fn a_variable_pyright_answers_for_is_edited_as_its_whole_assignment() {
    if !pyright_on_path() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo)
        .status()
        .expect("git");
    assert!(status.success());
    std::fs::write(
        repo.join("svc.py"),
        "class Service:\n    timeout = 30\n\n    def start(self):\n        return self.timeout\n\n\n\
         LIMIT = 5\nunused = shared = dict(\n    a=1,\n)\nTOTAL = Service.timeout = 60\n",
    )
    .unwrap();
    let run = |args: &[&str]| -> Value {
        let output = symora()
            .args(args)
            .args(["--format", "compact"])
            .current_dir(repo)
            .env("SYMORA_NO_DAEMON", "1")
            .stderr(Stdio::null())
            .output()
            .expect("run symora");
        common::parse_response(&output.stdout).expect("JSON on stdout")
    };

    let listed = run(&["symbols", "svc.py"]);
    assert_eq!(listed["backend"], "document", "pyright answered: {listed}");
    for (symbol, line) in [("Service/timeout", 2), ("LIMIT", 8)] {
        let preview = run(&["edit", "delete", "svc.py", "--symbol", symbol, "--dry-run"]);
        assert_eq!(preview["lines"]["start"], line, "{symbol}: {preview}");
        assert_eq!(preview["lines"]["end"], line, "{symbol}: {preview}");
    }
    let inserted = run(&[
        "edit",
        "insert-after",
        "svc.py",
        "--symbol",
        "shared",
        "--code",
        "OTHER = 1",
        "--dry-run",
    ]);
    assert_eq!(inserted["lines"]["start"], 9, "{inserted}");
    assert_eq!(inserted["lines"]["end"], 11, "{inserted}");
    let refused = run(&[
        "edit",
        "delete",
        "svc.py",
        "--symbol",
        "unused",
        "--dry-run",
    ]);
    assert_eq!(refused["error"]["code"], "unsupported", "{refused}");
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("'shared'"), "{refused}");
    let refused = run(&["edit", "delete", "svc.py", "--symbol", "TOTAL", "--dry-run"]);
    assert_eq!(refused["error"]["code"], "unsupported", "{refused}");
}

/// Pyright never confirms workspace readiness, so with no index a symbol
/// search rests on an answer it cannot vouch for, and the index it extracts
/// Python into is what to build.
#[test]
#[ignore = "requires pyright; run: cargo test --test lang_fixtures -- --ignored"]
fn an_unbuilt_index_is_advised_when_pyright_answers_alone() {
    if !pyright_on_path() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("target.py"),
        "def target():\n    return 1\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("pyrightconfig.json"),
        r#"{"include":["."]}"#,
    )
    .unwrap();
    let output = symora()
        .args(["search", "symbols", "target", "--lang", "python"])
        .args(["--format", "compact"])
        .current_dir(root.path())
        .env("SYMORA_NO_DAEMON", "1")
        .output()
        .unwrap();
    let result: Value = common::parse_response(&output.stdout).unwrap();
    assert!(output.status.success(), "{result}");
    assert_eq!(result["indexing"], "unconfirmed", "{result}");
    assert!(
        result["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "symora search index build"),
        "{result}"
    );
}

#[test]
#[ignore = "requires pyright; run: cargo test --test lang_fixtures -- --ignored"]
fn pyright_rename_applies_only_once_its_unconfirmed_edits_are_accepted() {
    if !pyright_on_path() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let files = [
        ("target.py", "def target():\n    return 1\n"),
        ("caller.py", "from target import target\nvalue = target()\n"),
    ];
    for (name, content) in files {
        std::fs::write(root.path().join(name), content).unwrap();
    }
    std::fs::write(
        root.path().join("pyrightconfig.json"),
        r#"{"include":["."]}"#,
    )
    .unwrap();
    let rename = |flag: Option<&str>| {
        let mut command = symora();
        command
            .args([
                "rename",
                "target.py:1:5",
                "renamed_target",
                "--format",
                "compact",
            ])
            .args(flag)
            .current_dir(root.path())
            .env("SYMORA_NO_DAEMON", "1");
        let output = command.output().unwrap();
        let result: Value = common::parse_response(&output.stdout).unwrap();
        (output.status.success(), result)
    };
    let read = |name: &str| std::fs::read_to_string(root.path().join(name)).unwrap();

    let (ok, preview) = rename(Some("--dry-run"));
    assert!(ok, "{preview}");
    assert_eq!(preview["indexing"], "unconfirmed", "{preview}");
    assert_eq!(preview["affected_files"], 2, "{preview}");

    let (ok, refusal) = rename(None);
    assert!(!ok, "{refusal}");
    assert_eq!(refusal["error"]["code"], "precondition_failed", "{refusal}");
    for (name, content) in files {
        assert_eq!(read(name), content);
    }

    let (ok, applied) = rename(Some("--accept-unconfirmed"));
    assert!(ok, "{applied}");
    assert_eq!(applied["indexing"], "unconfirmed", "{applied}");
    assert_eq!(read("target.py"), "def renamed_target():\n    return 1\n");
    assert_eq!(
        read("caller.py"),
        "from target import renamed_target\nvalue = renamed_target()\n"
    );
}
