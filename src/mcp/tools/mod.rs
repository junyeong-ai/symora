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

/// Enforce the `additionalProperties: false` every catalog schema
/// advertises: an argument key the schema doesn't declare is rejected
/// with the accepted set, never silently dropped — a typo'd key that
/// silently falls back to another addressing mode would edit the wrong
/// target. Null/absent arguments (the zero-argument call shape) pass
/// through; non-object shapes fall to the handler's own parsing.
fn check_unknown_arguments(tool: &ToolDefinition, arguments: &Value) -> Result<(), OutputError> {
    let Some(args) = arguments.as_object() else {
        return Ok(());
    };
    let Some(props) = tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    if let Some(unknown) = args.keys().find(|k| !props.contains_key(k.as_str())) {
        let mut accepted: Vec<&str> = props.keys().map(String::as_str).collect();
        accepted.sort_unstable();
        return Err(OutputError::invalid(format!(
            "Unknown argument '{unknown}' for tool '{}'",
            tool.name
        ))
        .with_hint(format!("Accepted arguments: {}", accepted.join(", "))));
    }
    Ok(())
}

pub async fn dispatch(
    name: &str,
    arguments: Value,
    app: &App,
    profile: McpProfile,
) -> Result<ToolOutput, OutputError> {
    if let Some(tool) = catalog().iter().find(|t| t.name == name) {
        // The profile gates calls, not just the advertised list — a hidden
        // tool that still dispatches would make the boundary cosmetic.
        if !profile.allows(tool) {
            return Err(OutputError::unsupported(format!(
                "Tool '{name}' is excluded by the read-only profile"
            )));
        }
        check_unknown_arguments(tool, &arguments)?;
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
            "find_call_path",
            "find_implementations",
            "get_hover",
            "get_context",
            "get_impact",
            "get_diagnostics",
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

    /// Every tool that advertises an output schema must stay error-tolerant: no
    /// `additionalProperties: false` (which would reject the handled-failure
    /// `{ "error": ... }` envelope and any forward-compatible field) and an
    /// explicit `error` property — so a tool's own error response always
    /// validates against its declared output schema.
    #[test]
    fn output_schemas_accommodate_the_error_envelope() {
        for tool in catalog() {
            let Some(schema) = &tool.output_schema else {
                continue;
            };
            assert_ne!(
                schema.get("additionalProperties"),
                Some(&serde_json::json!(false)),
                "{}: output schema must not set additionalProperties:false — it would \
                 reject the tool's own {{error}} envelope",
                tool.name,
            );
            assert!(
                schema
                    .get("properties")
                    .and_then(|p| p.get("error"))
                    .is_some(),
                "{}: output schema must declare an `error` property so a handled \
                 failure validates",
                tool.name,
            );
        }
    }

    fn tool(name: &str) -> &'static ToolDefinition {
        catalog()
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("tool catalog missing {name}"))
    }

    fn required_of(tool: &ToolDefinition) -> Vec<&str> {
        tool.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect()
    }

    /// The edit tools advertise both addressing modes, so neither `symbol` nor
    /// `line` is individually required — `file` is the only required BASE field
    /// (exactly-one-of is enforced in the handler). The writing tools also
    /// require their mandatory payload field (body/code), pinned separately in
    /// `mandatory_handler_fields_are_advertised_required`; `delete_symbol` has no
    /// extra mandatory field, so its required set is exactly `[file]`.
    #[test]
    fn edit_tools_advertise_symbol_addressing() {
        for name in [
            "replace_symbol_body",
            "insert_before_symbol",
            "insert_after_symbol",
            "delete_symbol",
        ] {
            let tool = tool(name);
            let required = required_of(tool);
            assert!(required.contains(&"file"), "{name}: must require file");
            assert!(
                !required.contains(&"symbol") && !required.contains(&"line"),
                "{name}: neither symbol nor line is individually required (exactly-one-of)"
            );
            let props = tool.input_schema["properties"].as_object().unwrap();
            for prop in ["symbol", "line", "column"] {
                assert!(props.contains_key(prop), "{name}: missing property {prop}");
            }
        }
        assert_eq!(
            required_of(tool("delete_symbol")),
            ["file"],
            "delete_symbol has no extra mandatory field, so required is exactly [file]"
        );
        for name in [
            "find_definition",
            "find_references",
            "find_callers",
            "find_callees",
            "find_call_path",
            "find_implementations",
            "get_hover",
            "get_context",
            "get_impact",
            "rename_symbol",
            "list_code_actions",
            "apply_code_action",
        ] {
            assert!(
                required_of(tool(name)).contains(&"line"),
                "{name}: location tools keep requiring line"
            );
        }
    }

    /// The location schema is split by omitted-column semantics: symbol-level
    /// tools address the symbol on the line for an omitted column, position-exact
    /// tools resolve at the literal column. The two families share an identical
    /// `{file, line, column}` shape, so only the `column` description tells them
    /// apart — this biconditional pins each tool to its family, so a tool wired
    /// to the wrong schema fails here instead of drifting silently.
    #[test]
    fn location_tools_advertise_their_column_semantics() {
        let column_desc = |name: &str| -> String {
            tool(name).input_schema["properties"]["column"]["description"]
                .as_str()
                .unwrap_or_else(|| panic!("{name}: column property missing a description"))
                .to_string()
        };

        // Position-exact: an omitted column resolves at the literal column.
        for name in [
            "find_definition",
            "get_hover",
            "rename_symbol",
            "list_code_actions",
            "apply_code_action",
        ] {
            let desc = column_desc(name);
            assert!(
                desc.contains("does not resolve to a symbol"),
                "{name}: position-exact tool must advertise position-exact column semantics, got: {desc}",
            );
        }

        // Symbol-level: an omitted column addresses the symbol on the line.
        for name in [
            "find_references",
            "find_callers",
            "find_callees",
            "find_call_path",
            "find_implementations",
            "get_context",
            "get_impact",
        ] {
            let desc = column_desc(name);
            assert!(
                desc.contains("address the symbol on the line"),
                "{name}: symbol-level tool must advertise symbol-level column semantics, got: {desc}",
            );
        }
    }

    /// Every tool whose handler input has a mandatory (non-Option, no-default)
    /// field beyond the base file/line must advertise it as required — otherwise
    /// a schema-conformant client omits it and hits a runtime "missing field"
    /// error from deserialization. Pins the whole class so a new `with_extra`
    /// mandatory field cannot silently regress to optional.
    #[test]
    fn mandatory_handler_fields_are_advertised_required() {
        for (name, field) in [
            ("find_call_path", "to"),
            ("rename_symbol", "new_name"),
            ("apply_code_action", "title"),
            ("replace_symbol_body", "body"),
            ("insert_before_symbol", "code"),
            ("insert_after_symbol", "code"),
        ] {
            assert!(
                required_of(tool(name)).contains(&field),
                "{name}: handler field `{field}` is non-Option but the schema does not mark it required"
            );
        }
    }

    #[test]
    fn unknown_arguments_are_rejected_structurally() {
        use serde_json::json;

        let err = check_unknown_arguments(
            tool("delete_symbol"),
            &json!({"file": "a.rs", "symbol": "Foo", "bogus": 1}),
        )
        .unwrap_err();
        assert!(matches!(err.code, crate::cli::ErrorCode::InvalidArgument));
        assert!(err.message.contains("bogus"));
        assert!(err.hint.unwrap().contains("file"));

        assert!(
            check_unknown_arguments(
                tool("delete_symbol"),
                &json!({"file": "a.rs", "symbol": "Foo"})
            )
            .is_ok()
        );
        assert!(check_unknown_arguments(tool("delete_symbol"), &Value::Null).is_ok());
        assert!(
            check_unknown_arguments(tool("get_project_overview"), &json!({"anything": true}))
                .is_err()
        );
        assert!(
            check_unknown_arguments(
                tool("get_file_overview"),
                &json!({"path": "a.rs", "depth": 2, "related_limit": 4}),
            )
            .is_ok()
        );
    }

    /// The fields a handler input struct deserializes and the properties
    /// its catalog schema advertises must be the same set, in both
    /// directions: `check_unknown_arguments` rejects undeclared keys, so
    /// an unadvertised field would be unreachable at runtime, and an
    /// advertised property no struct consumes would be silently dropped
    /// by serde. Walks the whole catalog so no tool can drift silently.
    #[test]
    fn handler_input_fields_match_advertised_properties_exactly() {
        for tool in catalog() {
            let fields = handlers::input_fields(tool.name)
                .unwrap_or_else(|| panic!("{}: no input-field row in handlers.rs", tool.name));
            let props = tool.input_schema["properties"].as_object().unwrap();
            let mut consumed: Vec<&str> = fields.to_vec();
            consumed.sort_unstable();
            let mut advertised: Vec<&str> = props.keys().map(String::as_str).collect();
            advertised.sort_unstable();
            assert_eq!(
                consumed, advertised,
                "{}: handler input fields and advertised catalog properties \
                 must be the same set",
                tool.name,
            );
        }
    }

    /// The complementary direction: an args object populating every
    /// advertised property (dummy value per declared type) must
    /// deserialize into the handler's input struct. A required struct
    /// field missing from the catalog (and the `input_fields` registry)
    /// fails here as a missing-field error; so does a type mismatch
    /// between a property's declared type and its struct field.
    #[test]
    fn advertised_properties_deserialize_into_handler_inputs() {
        use serde_json::json;

        for tool in catalog() {
            let props = tool.input_schema["properties"].as_object().unwrap();
            let mut args = serde_json::Map::new();
            for (name, schema) in props {
                let value = match (tool.name, name.as_str()) {
                    // Enum-valued string: an arbitrary dummy string is not
                    // a variant, so use one.
                    ("build_context_pack", "shape") => json!("json"),
                    _ => match schema["type"].as_str().unwrap() {
                        "string" => json!("dummy"),
                        "integer" => json!(1),
                        "boolean" => json!(true),
                        "array" => json!([]),
                        other => panic!("{}.{name}: unhandled property type {other}", tool.name),
                    },
                };
                args.insert(name.clone(), value);
            }
            handlers::deserialize_input(tool.name, Value::Object(args))
                .unwrap_or_else(|| panic!("{}: no input-deserialize row in handlers.rs", tool.name))
                .unwrap_or_else(|e| {
                    panic!(
                        "{}: advertised properties failed to deserialize into the \
                         handler input struct: {e}",
                        tool.name
                    )
                });
        }
    }
}
