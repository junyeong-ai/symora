use std::sync::LazyLock;

use serde_json::Value;

use crate::app::App;
use crate::cli::OutputError;

use super::McpProfile;
use super::protocol::{Content, ToolDefinition};

mod catalog;
mod handlers;
mod schema;

static CATALOG: LazyLock<Vec<ToolDefinition>> = LazyLock::new(catalog::build_catalog);

pub fn catalog() -> &'static [ToolDefinition] {
    &CATALOG
}

/// The catalog filtered to what `profile` exposes. `catalog()` stays the
/// single unfiltered source; this is a pure view over it.
pub fn visible_catalog(profile: McpProfile) -> Vec<ToolDefinition> {
    catalog()
        .iter()
        .filter(|tool| profile.allows(tool))
        .cloned()
        .collect()
}

/// Tool result for the MCP `tools/call` response: the captured command
/// output, the parsed JSON for `structuredContent`, and the truthful
/// `isError` flag.
pub struct ToolOutput {
    pub content: Vec<Content>,
    pub structured: Option<Value>,
    pub is_error: bool,
}

pub async fn dispatch(
    name: &str,
    arguments: Value,
    app: &App,
    profile: McpProfile,
) -> Result<ToolOutput, OutputError> {
    // The profile gates calls, not just the advertised list — a hidden
    // tool that still dispatches would make the boundary cosmetic.
    if let Some(tool) = catalog().iter().find(|t| t.name == name)
        && !profile.allows(tool)
    {
        return Err(OutputError::unsupported(format!(
            "Tool '{name}' is excluded by the read-only profile"
        )));
    }

    let captured = handlers::dispatch(name, arguments, app)
        .await
        .map_err(OutputError::from)?;
    // Single JSON object bodies double as `structuredContent`; text shapes
    // (e.g. pack --shape markdown) stay text-only.
    let structured = serde_json::from_str::<Value>(&captured.body)
        .ok()
        .filter(Value::is_object);
    Ok(ToolOutput {
        content: vec![Content::text(captured.body)],
        structured,
        is_error: captured.errored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_all_dispatchable_tools() {
        let names: Vec<&str> = catalog().iter().map(|t| t.name).collect();
        for name in &names {
            assert!(!name.is_empty());
        }
        for required in [
            "get_project_overview",
            "get_file_overview",
            "search_symbols",
            "search_content",
            "list_file_symbols",
            "inspect_symbol",
            "find_definition",
            "find_references",
            "find_callers",
            "find_callees",
            "find_implementations",
            "get_hover",
            "get_context",
            "get_impact",
            "build_context_pack",
            "rename_symbol",
            "list_code_actions",
            "apply_code_action",
            "replace_symbol_body",
            "insert_before_symbol",
            "insert_after_symbol",
            "delete_symbol",
        ] {
            assert!(names.contains(&required), "tool catalog missing {required}");
        }
    }

    /// The human-readable mutation warning and the typed annotation are
    /// one fact stated twice; this biconditional keeps them from
    /// drifting in either direction across the whole catalog.
    #[test]
    fn mutation_warnings_and_annotations_agree() {
        for tool in catalog() {
            let advertised = tool.description.contains("Mutates");
            let mutating = !tool.annotations.read_only_hint;
            assert_eq!(
                advertised, mutating,
                "{}: description 'Mutates' marker and readOnlyHint disagree",
                tool.name,
            );
        }
    }

    #[test]
    fn read_only_profile_filters_exactly_the_mutating_tools() {
        let visible = visible_catalog(McpProfile::ReadOnly);
        assert!(visible.iter().all(|t| t.annotations.read_only_hint));
        let hidden = catalog().len() - visible.len();
        let mutating = catalog()
            .iter()
            .filter(|t| !t.annotations.read_only_hint)
            .count();
        assert_eq!(hidden, mutating);
        assert!(mutating >= 5, "expected the editing tools to be mutating");
    }
}
