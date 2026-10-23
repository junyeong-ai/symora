use std::path::PathBuf;
use std::process::Command;

pub const SYMORA: &str = env!("CARGO_BIN_EXE_symora");

/// A `symora` command that reads its global configuration from a directory
/// holding none, so a setting in the developer's own `~/.config/symora`
/// cannot change what a test observes. A test that needs a global
/// configuration sets its own `XDG_CONFIG_HOME` over this one.
pub fn symora() -> Command {
    let mut command = Command::new(SYMORA);
    command.env("XDG_CONFIG_HOME", no_global_config());
    command
}

pub fn no_global_config() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-global-config")
}

/// The fields that qualify a list (`.claude/rules/json-output-stability.md`).
const QUALIFIERS: &[&str] = &[
    "config_errors",
    "error",
    "truncated",
    "stale",
    "incomplete",
    "indexing",
    "coverage_gaps",
    "unread_paths",
    "unmeasured_files",
    "bodies_included",
    "body_omitted",
    "refs_status",
    "callers_status",
    "hints",
    "next_commands",
];

/// Every qualifier in an object precedes the object's lists, so a reader
/// that sees only the head of a response still has what qualifies them.
pub fn assert_qualifiers_lead(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut first_list: Option<&str> = None;
            for (key, child) in map {
                if QUALIFIERS.contains(&key.as_str()) {
                    if let Some(list) = first_list {
                        panic!("`{key}` follows the list `{list}`: {value}");
                    }
                } else if child.is_array() {
                    first_list.get_or_insert(key);
                }
                assert_qualifiers_lead(child);
            }
        }
        serde_json::Value::Array(values) => values.iter().for_each(assert_qualifiers_lead),
        _ => {}
    }
}

/// Parse a response and hold it to the qualifier-order contract.
pub fn parse_response(stdout: &[u8]) -> serde_json::Result<serde_json::Value> {
    let value = serde_json::from_slice(stdout)?;
    assert_qualifiers_lead(&value);
    Ok(value)
}
