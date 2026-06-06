use std::fmt;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::LocationArg;

pub fn schema_object(fields: &[(&str, &str, &str)], required: &[&str]) -> Value {
    let mut props = serde_json::Map::new();
    for (name, ty, desc) in fields {
        props.insert(
            (*name).to_string(),
            json!({ "type": ty, "description": desc }),
        );
    }
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn location_schema() -> Value {
    schema_object(
        &[
            ("file", "string", "Project-relative file path"),
            ("line", "integer", "1-indexed line number"),
            ("column", "integer", "1-indexed column number (default 1)"),
        ],
        &["file", "line"],
    )
}

/// Output envelope for list-shaped tools — the `Section<T>` contract.
/// `items` stays loosely typed on purpose: overclaiming the item shape
/// here would drift from the real output types, and a wrong schema is
/// worse than a loose one. Nothing is `required` because a failed call
/// emits `{ "error": ... }` instead of the list fields.
pub fn section_output_schema(items_description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer", "description": "Total matches found" },
            "showing": { "type": "integer", "description": "Number emitted in items" },
            "items": {
                "type": "array",
                "items": { "type": "object" },
                "description": items_description,
            },
            "truncated": {
                "type": "boolean",
                "description": "Present (and true) only when showing < count",
            },
            "stale": {
                "type": "boolean",
                "description": "Present (and true) only when index-served rows \
                                came from files that changed on disk since \
                                indexing — rebuild the index to refresh",
            },
            "hints": { "type": "array", "items": { "type": "string" } },
            "next_commands": { "type": "array", "items": { "type": "string" } },
            "indexing": {
                "type": "string",
                "enum": ["timed_out"],
                "description": "Present only when computed under degraded \
                                workspace indexing — results are a lower bound",
            },
            "error": {
                "type": "object",
                "description": "Structured failure: { code, message, hint? }",
            },
        },
    })
}

pub fn with_extra(mut base: Value, extras: &[(&str, &str, &str)]) -> Value {
    let props = base
        .get_mut("properties")
        .and_then(|v| v.as_object_mut())
        .expect("base schema has properties");
    for (name, ty, desc) in extras {
        props.insert(
            (*name).to_string(),
            json!({ "type": ty, "description": desc }),
        );
    }
    base
}

#[derive(Deserialize)]
pub struct LocationInput {
    pub file: String,
    pub line: u32,
    #[serde(default = "default_column")]
    pub column: u32,
}

fn default_column() -> u32 {
    1
}

impl LocationInput {
    pub fn into_arg(self) -> LocationArg {
        LocationArg {
            location: self.to_string(),
        }
    }
}

impl fmt::Display for LocationInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_schema_requires_file_and_line() {
        let schema = location_schema();
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"file"));
        assert!(names.contains(&"line"));
        assert!(!names.contains(&"column"));
    }

    #[test]
    fn with_extra_appends_to_properties() {
        let extended = with_extra(
            location_schema(),
            &[("limit", "integer", "Maximum results")],
        );
        let props = extended["properties"].as_object().unwrap();
        assert!(props.contains_key("file"));
        assert!(props.contains_key("limit"));
    }

    #[test]
    fn location_input_defaults_column_to_one() {
        let input: LocationInput =
            serde_json::from_value(json!({"file": "src/main.rs", "line": 10})).unwrap();
        assert_eq!(input.column, 1);
        assert_eq!(input.into_arg().location, "src/main.rs:10:1");
    }
}
