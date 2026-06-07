//! Config-format writers for the MCP installer. Each is a pure
//! `existing -> new` transform plus an atomic on-disk landing, so the host
//! targets stay declarative and the transforms are unit-testable without
//! touching a real config.
//!
//! Two invariants hold across formats:
//! - **Idempotent**: writing an entry that already matches is a no-op
//!   (`changed = false`), so a configured machine yields byte-identical
//!   files across runs and never triggers a needless write.
//! - **Surgical**: only the `symora` entry is ever touched; sibling servers,
//!   tables, keys, and (for TOML) comments survive verbatim.

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::host::ServerSpec;

/// The result of a config transform: the bytes to land and whether they
/// differ from what was there. `now_empty` reports that the config no
/// longer carries any meaningful entry, so a file the installer created can
/// be removed without a trace rather than left as an empty husk.
pub struct Edit {
    pub content: String,
    pub changed: bool,
    pub now_empty: bool,
}

fn server_object(spec: &ServerSpec) -> Value {
    json!({ "command": spec.command, "args": spec.args })
}

// ---------------------------------------------------------------------------
// JSON (Claude Code `.mcp.json`)
// ---------------------------------------------------------------------------

/// Upsert `mcpServers.<name>` into a JSON config, preserving every other
/// key. An empty input starts from `{}`. A non-object root or a non-object
/// `mcpServers` is a malformed config we refuse to rewrite (the caller skips
/// the host rather than clobbering it).
pub fn json_upsert(existing: &str, name: &str, spec: &ServerSpec) -> Result<Edit> {
    let mut root = parse_json_root(existing)?;
    let desired = server_object(spec);

    let already = root
        .get("mcpServers")
        .and_then(|servers| servers.get(name))
        .is_some_and(|current| current == &desired);
    if already {
        return Ok(Edit {
            content: existing.to_string(),
            changed: false,
            now_empty: false,
        });
    }

    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    servers
        .as_object_mut()
        .ok_or_else(|| anyhow!("`mcpServers` is not a JSON object"))?
        .insert(name.to_string(), desired);

    Ok(Edit {
        content: render_json(&root),
        changed: true,
        now_empty: false,
    })
}

/// Remove `mcpServers.<name>` from a JSON config. Prunes an emptied
/// `mcpServers` wrapper and reports `now_empty` when nothing meaningful is
/// left, so the caller can delete a file it created.
pub fn json_remove(existing: &str, name: &str) -> Result<Edit> {
    let mut root = parse_json_root(existing)?;

    let removed = root
        .get_mut("mcpServers")
        .and_then(|servers| servers.as_object_mut())
        .is_some_and(|servers| servers.remove(name).is_some());
    if !removed {
        return Ok(Edit {
            content: existing.to_string(),
            changed: false,
            now_empty: root_is_empty(&root),
        });
    }

    if root
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        root.remove("mcpServers");
    }

    Ok(Edit {
        content: render_json(&root),
        changed: true,
        now_empty: root_is_empty(&root),
    })
}

fn parse_json_root(existing: &str) -> Result<Map<String, Value>> {
    if existing.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(existing)? {
        Value::Object(map) => Ok(map),
        _ => Err(anyhow!("config root is not a JSON object")),
    }
}

fn root_is_empty(root: &Map<String, Value>) -> bool {
    root.is_empty()
}

fn render_json(root: &Map<String, Value>) -> String {
    let mut content = serde_json::to_string_pretty(&Value::Object(root.clone()))
        .expect("a JSON object always serializes");
    content.push('\n');
    content
}

// ---------------------------------------------------------------------------
// TOML (Codex `config.toml`)
// ---------------------------------------------------------------------------

/// Upsert the `[mcp_servers.<name>]` table via `toml_edit`, which preserves
/// the rest of the document — comments, ordering, and every sibling table —
/// byte-for-byte, and correctly recognizes any spelling of an existing entry
/// (canonical header, quoted key, dotted key, inline table) so it is updated
/// in place rather than duplicated. A malformed config is refused, not
/// rewritten.
pub fn toml_upsert(existing: &str, name: &str, spec: &ServerSpec) -> Result<Edit> {
    let mut doc = parse_toml(existing)?;

    // Idempotency: if our entry already matches, leave the file untouched so
    // a configured machine yields byte-identical bytes across runs.
    if let Some(current) = doc.get("mcp_servers").and_then(|servers| servers.get(name))
        && toml_entry_matches(current, spec)
    {
        return Ok(Edit {
            content: existing.to_string(),
            changed: false,
            now_empty: false,
        });
    }

    let servers = doc
        .entry("mcp_servers")
        .or_insert(Item::Table(Table::new()));
    let servers = servers
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("`mcp_servers` is not a table"))?;
    servers.insert(name, Item::Table(server_table(spec)));

    let content = doc.to_string();
    Ok(Edit {
        changed: content != existing,
        content,
        now_empty: false,
    })
}

/// Remove the `[mcp_servers.<name>]` entry (any spelling), pruning an emptied
/// `mcp_servers` wrapper, and leaving the rest of the document verbatim.
pub fn toml_remove(existing: &str, name: &str) -> Result<Edit> {
    let mut doc = parse_toml(existing)?;

    let removed = doc
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
        .is_some_and(|servers| servers.remove(name).is_some());
    if !removed {
        return Ok(Edit {
            content: existing.to_string(),
            changed: false,
            now_empty: doc.as_table().is_empty(),
        });
    }

    if doc
        .get("mcp_servers")
        .and_then(Item::as_table_like)
        .is_some_and(toml_edit::TableLike::is_empty)
    {
        doc.as_table_mut().remove("mcp_servers");
    }

    let content = doc.to_string();
    Ok(Edit {
        now_empty: doc.as_table().is_empty(),
        changed: content != existing,
        content,
    })
}

fn parse_toml(existing: &str) -> Result<DocumentMut> {
    if existing.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    existing
        .parse::<DocumentMut>()
        .map_err(|e| anyhow!("config is not valid TOML: {e}"))
}

fn server_table(spec: &ServerSpec) -> Table {
    let mut table = Table::new();
    table.insert("command", value(spec.command.as_str()));
    let mut args = Array::new();
    for arg in &spec.args {
        args.push(arg.as_str());
    }
    table.insert("args", value(args));
    table
}

fn toml_entry_matches(item: &Item, spec: &ServerSpec) -> bool {
    let Some(table) = item.as_table_like() else {
        return false;
    };
    let command_ok = table.get("command").and_then(Item::as_str) == Some(spec.command.as_str());
    let args_ok = table
        .get("args")
        .and_then(Item::as_array)
        .is_some_and(|args| {
            args.len() == spec.args.len()
                && args
                    .iter()
                    .zip(&spec.args)
                    .all(|(got, want)| got.as_str() == Some(want.as_str()))
        });
    command_ok && args_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServerSpec {
        ServerSpec {
            command: "/usr/local/bin/symora".to_string(),
            args: vec!["mcp".to_string(), "serve".to_string()],
        }
    }

    #[test]
    fn json_creates_from_empty() {
        let edit = json_upsert("", "symora", &spec()).unwrap();
        assert!(edit.changed);
        let parsed: Value = serde_json::from_str(&edit.content).unwrap();
        assert_eq!(
            parsed["mcpServers"]["symora"]["command"],
            "/usr/local/bin/symora"
        );
        assert_eq!(parsed["mcpServers"]["symora"]["args"][0], "mcp");
    }

    #[test]
    fn json_upsert_is_idempotent() {
        let first = json_upsert("", "symora", &spec()).unwrap();
        let second = json_upsert(&first.content, "symora", &spec()).unwrap();
        assert!(
            !second.changed,
            "re-applying the same entry must not change bytes"
        );
        assert_eq!(first.content, second.content);
    }

    #[test]
    fn json_preserves_sibling_servers() {
        let existing = r#"{"mcpServers":{"other":{"command":"x","args":[]}}}"#;
        let edit = json_upsert(existing, "symora", &spec()).unwrap();
        let parsed: Value = serde_json::from_str(&edit.content).unwrap();
        assert_eq!(parsed["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            parsed["mcpServers"]["symora"]["command"],
            "/usr/local/bin/symora"
        );
    }

    #[test]
    fn json_remove_prunes_and_reports_empty() {
        let with = json_upsert("", "symora", &spec()).unwrap();
        let without = json_remove(&with.content, "symora").unwrap();
        assert!(without.changed);
        assert!(
            without.now_empty,
            "a config holding only symora is empty after removal"
        );
        let parsed: Value = serde_json::from_str(&without.content).unwrap();
        assert!(parsed.get("mcpServers").is_none());
    }

    #[test]
    fn json_remove_keeps_siblings_and_is_not_empty() {
        let existing = r#"{"mcpServers":{"other":{"command":"x","args":[]},"symora":{"command":"y","args":[]}}}"#;
        let edit = json_remove(existing, "symora").unwrap();
        assert!(edit.changed);
        assert!(!edit.now_empty);
        let parsed: Value = serde_json::from_str(&edit.content).unwrap();
        assert_eq!(parsed["mcpServers"]["other"]["command"], "x");
        assert!(parsed["mcpServers"].get("symora").is_none());
    }

    #[test]
    fn json_remove_absent_is_noop() {
        let existing = r#"{"mcpServers":{"other":{"command":"x","args":[]}}}"#;
        let edit = json_remove(existing, "symora").unwrap();
        assert!(!edit.changed);
    }

    #[test]
    fn json_refuses_malformed_root() {
        assert!(json_upsert("[1,2,3]", "symora", &spec()).is_err());
        assert!(json_upsert("not json", "symora", &spec()).is_err());
    }

    // TOML assertions parse the result rather than match formatting, so they
    // pin behavior, not `toml_edit`'s whitespace choices.
    fn toml_command(content: &str, name: &str) -> Option<String> {
        let doc: DocumentMut = content.parse().ok()?;
        doc.get("mcp_servers")?
            .get(name)?
            .as_table_like()?
            .get("command")?
            .as_str()
            .map(str::to_string)
    }

    fn toml_table_count(content: &str) -> usize {
        content.matches("[mcp_servers.symora]").count()
    }

    #[test]
    fn toml_creates_from_empty() {
        let edit = toml_upsert("", "symora", &spec()).unwrap();
        assert!(edit.changed);
        assert_eq!(
            toml_command(&edit.content, "symora").as_deref(),
            Some("/usr/local/bin/symora")
        );
        let doc: DocumentMut = edit.content.parse().unwrap();
        let args = doc["mcp_servers"]["symora"]["args"].as_array().unwrap();
        let got: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(got, ["mcp", "serve"]);
    }

    #[test]
    fn toml_upsert_is_idempotent() {
        let first = toml_upsert("", "symora", &spec()).unwrap();
        let second = toml_upsert(&first.content, "symora", &spec()).unwrap();
        assert!(!second.changed);
        assert_eq!(first.content, second.content);
    }

    #[test]
    fn toml_preserves_comments_and_sibling_tables() {
        let existing = "# my codex config\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"x\"\nargs = []\n";
        let edit = toml_upsert(existing, "symora", &spec()).unwrap();
        assert!(edit.changed);
        assert!(
            edit.content.contains("# my codex config"),
            "comment must survive"
        );
        assert!(edit.content.contains("model = \"gpt-5\""));
        assert_eq!(toml_command(&edit.content, "other").as_deref(), Some("x"));
        assert_eq!(
            toml_command(&edit.content, "symora").as_deref(),
            Some("/usr/local/bin/symora")
        );
    }

    #[test]
    fn toml_update_replaces_only_our_table() {
        let existing = "[mcp_servers.symora]\ncommand = \"old\"\nargs = []\n\n[other]\nk = 1\n";
        let edit = toml_upsert(existing, "symora", &spec()).unwrap();
        assert!(edit.changed);
        assert_eq!(
            toml_command(&edit.content, "symora").as_deref(),
            Some("/usr/local/bin/symora")
        );
        let doc: DocumentMut = edit.content.parse().unwrap();
        assert_eq!(doc["other"]["k"].as_integer(), Some(1));
    }

    /// The bug all three reviewers flagged: a non-canonical existing header
    /// (trailing comment, here) must be UPDATED in place, never duplicated
    /// into a second `[mcp_servers.symora]` that would make the file invalid.
    #[test]
    fn toml_updates_non_canonical_existing_entry_without_duplicating() {
        let existing = "[mcp_servers.symora] # mine\ncommand = \"old\"\nargs = []\n";
        let edit = toml_upsert(existing, "symora", &spec()).unwrap();
        assert!(edit.changed);
        assert_eq!(
            toml_command(&edit.content, "symora").as_deref(),
            Some("/usr/local/bin/symora")
        );
        assert_eq!(
            toml_table_count(&edit.content),
            1,
            "must not append a duplicate table"
        );
        // Result is valid TOML.
        assert!(edit.content.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn toml_remove_excises_table_and_keeps_rest() {
        let existing = "model = \"gpt-5\"\n\n[mcp_servers.symora]\ncommand = \"x\"\nargs = []\n";
        let edit = toml_remove(existing, "symora").unwrap();
        assert!(edit.changed);
        assert!(toml_command(&edit.content, "symora").is_none());
        assert!(edit.content.contains("model = \"gpt-5\""));
        assert!(!edit.now_empty);
    }

    #[test]
    fn toml_remove_reports_blank_when_only_our_table_existed() {
        let existing = "[mcp_servers.symora]\ncommand = \"x\"\nargs = []\n";
        let edit = toml_remove(existing, "symora").unwrap();
        assert!(edit.changed);
        assert!(edit.now_empty);
    }

    #[test]
    fn toml_remove_absent_is_noop() {
        let edit = toml_remove("model = \"gpt-5\"\n", "symora").unwrap();
        assert!(!edit.changed);
    }

    #[test]
    fn toml_refuses_malformed_config() {
        assert!(toml_upsert("this is = = not toml", "symora", &spec()).is_err());
    }

    #[test]
    fn toml_escapes_backslashes_in_paths() {
        let win = ServerSpec {
            command: r"C:\tools\symora.exe".to_string(),
            args: vec!["mcp".to_string(), "serve".to_string()],
        };
        let edit = toml_upsert("", "symora", &win).unwrap();
        assert_eq!(
            toml_command(&edit.content, "symora").as_deref(),
            Some(r"C:\tools\symora.exe")
        );
    }
}
