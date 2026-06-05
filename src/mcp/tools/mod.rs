use std::sync::LazyLock;

use serde_json::Value;

use crate::app::App;
use crate::cli::OutputError;

use super::protocol::{Content, ToolDefinition};

mod catalog;
mod handlers;
mod schema;

static CATALOG: LazyLock<Vec<ToolDefinition>> = LazyLock::new(catalog::build_catalog);

pub fn catalog() -> &'static [ToolDefinition] {
    &CATALOG
}

/// Tool result for the MCP `tools/call` response: the captured command
/// output, the parsed JSON for `structuredContent`, and the truthful
/// `isError` flag.
pub struct ToolOutput {
    pub content: Vec<Content>,
    pub structured: Option<Value>,
    pub is_error: bool,
}

pub async fn dispatch(name: &str, arguments: Value, app: &App) -> Result<ToolOutput, OutputError> {
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
        ] {
            assert!(names.contains(&required), "tool catalog missing {required}");
        }
    }

    #[test]
    fn mutating_tools_advertise_themselves_in_descriptions() {
        let mutating = [
            "rename_symbol",
            "apply_code_action",
            "replace_symbol_body",
            "insert_before_symbol",
            "insert_after_symbol",
        ];
        for name in mutating {
            let tool = catalog()
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            assert!(
                tool.description.contains("Mutates"),
                "{name} description should warn about mutation",
            );
        }
    }
}
