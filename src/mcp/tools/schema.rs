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

/// `file`/`line`/`column` location schema. The omitted-column semantics differ
/// by tool family, so the `column` description is supplied by the caller:
/// symbol-level tools address the symbol declared on the line, position-exact
/// tools resolve at the literal column — see [`location_schema`] and
/// [`position_exact_location_schema`].
fn location_schema_with(column_desc: &str) -> Value {
    schema_object(
        &[
            ("file", "string", "Project-relative file path"),
            ("line", "integer", "1-indexed line number"),
            ("column", "integer", column_desc),
        ],
        &["file", "line"],
    )
}

/// Location schema for the symbol-level tools — those that resolve the input
/// to the declaration it addresses (the inverse of
/// [`position_exact_location_schema`]).
pub fn location_schema() -> Value {
    location_schema_with(
        "1-indexed column: on a declaration it means that symbol, elsewhere the \
         token there (a call site resolves to the symbol called, as \
         find_definition reads it); omit to address the symbol on the line",
    )
}

/// Location schema for the position-exact tools — those whose handler sends the
/// literal column straight to the LSP request without resolving a symbol, so
/// an omitted column resolves at the line start rather than the symbol.
pub fn position_exact_location_schema() -> Value {
    location_schema_with(
        "1-indexed column (default: 1, the line start). Resolution is \
         position-exact and does not resolve to a symbol — pass the symbol's \
         column, since the line start is often whitespace",
    )
}

/// Input schema for the symbol-editing tools: `file` plus exactly one
/// of `symbol` or `line`. The flat schema stays oneOf-free on purpose —
/// the exactly-one-of rule is enforced in `EditTargetInput::into_target`
/// identically for every host, validating or not.
pub fn edit_target_schema() -> Value {
    schema_object(
        &[
            ("file", "string", "Project-relative file path"),
            (
                "symbol",
                "string",
                "Symbol path within the file (e.g. 'Class/method'), as returned by \
                 search_symbols or list_file_symbols. Pass exactly one of symbol or line.",
            ),
            (
                "line",
                "integer",
                "1-indexed line number of the symbol. Pass exactly one of symbol or line.",
            ),
            (
                "column",
                "integer",
                "1-indexed column, which must be on the symbol's declaration (its keyword \
                 through its name; a column inside a body is refused). Only valid together \
                 with line; omitted, the symbol declared on the line is targeted.",
            ),
        ],
        &["file"],
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
            "incomplete": {
                "type": "boolean",
                "description": "Present (and true) only when count is a lower \
                                bound rather than everything the answer's own \
                                sources held — a path they could not be read \
                                from, or a cap that stopped short of what they \
                                hold. The cause leads hints",
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

/// Loose output schema for a non-list tool: declares the success fields but
/// stays open — no `additionalProperties: false`, no `required` — and always
/// carries the shared `error` property, so a handled-failure `{ "error": ... }`
/// envelope validates the same as the success shape. The list-shaped analog is
/// `section_output_schema`; like it, the openness is deliberate — a strict
/// output schema that rejects the tool's own error envelope is worse than a
/// loose one.
pub fn output_schema(fields: &[(&str, &str, &str)]) -> Value {
    let mut props = serde_json::Map::new();
    for (name, ty, desc) in fields {
        props.insert(
            (*name).to_string(),
            json!({ "type": ty, "description": desc }),
        );
    }
    props.insert(
        "error".to_string(),
        json!({ "type": "object", "description": "Structured failure: { code, message, hint? }" }),
    );
    json!({ "type": "object", "properties": props })
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

/// Mark `names` as required on `base` (appending to its `required` array). A
/// property added via `with_extra` is optional by default; use this when the
/// handler's input struct makes it mandatory (e.g. find_call_path's `to`), so
/// the advertised schema stays in lockstep with deserialization.
pub fn with_required(mut base: Value, names: &[&str]) -> Value {
    let required = base
        .get_mut("required")
        .and_then(|v| v.as_array_mut())
        .expect("base schema has a required array");
    for name in names {
        required.push(Value::String((*name).to_string()));
    }
    base
}

#[derive(Deserialize)]
pub struct LocationInput {
    pub file: String,
    pub line: u32,
    /// Omitted addresses the symbol on the line; present targets a precise
    /// column. Kept optional so symbol-level tools (callers/callees/…) keep the
    /// CLI's line addressing instead of being pinned to column 1.
    #[serde(default)]
    pub column: Option<u32>,
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
        match self.column {
            Some(column) => write!(f, "{}:{}:{}", self.file, self.line, column),
            None => write!(f, "{}:{}", self.file, self.line),
        }
    }
}

#[derive(Deserialize)]
pub struct EditTargetInput {
    pub file: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub symbol: Option<String>,
}

impl EditTargetInput {
    /// Convert into the edit command layer's `(target, symbol)` pair,
    /// enforcing exactly one of `symbol` or `line`. Line mode produces
    /// the same `file:line[:column]` target string the CLI accepts — an
    /// omitted column stays omitted, so the shared resolution path keeps
    /// its line-addressed semantics (the symbol declared on the line)
    /// instead of being pinned to column 1.
    pub fn into_target(self) -> anyhow::Result<(String, Option<String>)> {
        match (self.line, self.symbol) {
            (Some(_), Some(_)) => Err(anyhow::Error::new(
                crate::cli::OutputError::invalid(
                    "Pass exactly one of 'symbol' or 'line', not both",
                )
                .with_hint(
                    "Address the target by a symbol path like 'Class/method', or by a \
                     1-indexed line — never both",
                ),
            )),
            (None, None) => {
                let message = if self.column.is_some() {
                    "'column' applies only with 'line'; pass exactly one of 'symbol' or 'line'"
                } else {
                    "Pass exactly one of 'symbol' or 'line'"
                };
                Err(anyhow::Error::new(
                    crate::cli::OutputError::invalid(message).with_hint(
                        "Take a symbol path from search_symbols or list_file_symbols, or a \
                         1-indexed line",
                    ),
                ))
            }
            (Some(line), None) => Ok((
                match self.column {
                    Some(column) => format!("{}:{}:{}", self.file, line, column),
                    None => format!("{}:{}", self.file, line),
                },
                None,
            )),
            (None, Some(symbol)) => {
                if self.column.is_some() {
                    Err(anyhow::Error::new(
                        crate::cli::OutputError::invalid(
                            "'column' applies only with 'line'; a symbol path already \
                             addresses the target",
                        )
                        .with_hint(
                            "Drop 'column' to address by symbol, or pass a 1-indexed line \
                             and column instead",
                        ),
                    ))
                } else {
                    Ok((self.file, Some(symbol)))
                }
            }
        }
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

    /// An omitted column stays omitted so the shared resolution path keeps its
    /// line-addressed semantics (the symbol on the line); an explicit column is
    /// carried through for position-precise targeting.
    #[test]
    fn location_input_omits_column_for_line_addressed_targeting() {
        let line_only: LocationInput =
            serde_json::from_value(json!({"file": "src/main.rs", "line": 10})).unwrap();
        assert_eq!(line_only.column, None);
        assert_eq!(line_only.into_arg().location, "src/main.rs:10");

        let with_column: LocationInput =
            serde_json::from_value(json!({"file": "src/main.rs", "line": 10, "column": 3}))
                .unwrap();
        assert_eq!(with_column.column, Some(3));
        assert_eq!(with_column.into_arg().location, "src/main.rs:10:3");
    }

    #[test]
    fn edit_target_schema_requires_only_file() {
        let schema = edit_target_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["file"]);
        let props = schema["properties"].as_object().unwrap();
        for prop in ["symbol", "line", "column"] {
            assert!(props.contains_key(prop), "missing property {prop}");
        }
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    /// Line mode builds the same `file:line[:column]` string the CLI
    /// accepts: an omitted column stays omitted (line-addressed
    /// resolution), an explicit one is carried through.
    #[test]
    fn edit_target_input_line_mode_builds_location_target() {
        let input: EditTargetInput =
            serde_json::from_value(json!({"file": "a.rs", "line": 10})).unwrap();
        assert_eq!(input.into_target().unwrap(), ("a.rs:10".to_string(), None));

        let input: EditTargetInput =
            serde_json::from_value(json!({"file": "a.rs", "line": 10, "column": 5})).unwrap();
        assert_eq!(
            input.into_target().unwrap(),
            ("a.rs:10:5".to_string(), None)
        );
    }

    #[test]
    fn edit_target_input_symbol_mode_passes_pattern() {
        let input: EditTargetInput =
            serde_json::from_value(json!({"file": "a.rs", "symbol": "Foo/bar"})).unwrap();
        assert_eq!(
            input.into_target().unwrap(),
            ("a.rs".to_string(), Some("Foo/bar".to_string()))
        );
    }

    fn expect_invalid_argument(input: serde_json::Value) -> crate::cli::OutputError {
        use crate::cli::{ErrorCode, OutputError};
        let input: EditTargetInput = serde_json::from_value(input).unwrap();
        let out: OutputError = input.into_target().unwrap_err().into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
        out
    }

    #[test]
    fn edit_target_input_rejects_both() {
        expect_invalid_argument(json!({"file": "a.rs", "line": 10, "symbol": "Foo/bar"}));
    }

    #[test]
    fn edit_target_input_rejects_neither() {
        let err = expect_invalid_argument(json!({"file": "a.rs"}));
        assert!(!err.message.contains("column"));
    }

    /// A stray `column` with neither `symbol` nor `line` is named in the
    /// refusal — the generic neither-arm message would hide the one
    /// argument the caller actually passed.
    #[test]
    fn edit_target_input_rejects_column_alone_naming_it() {
        let err = expect_invalid_argument(json!({"file": "a.rs", "column": 3}));
        assert!(err.message.contains("'column' applies only with 'line'"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn edit_target_input_rejects_column_with_symbol() {
        let err =
            expect_invalid_argument(json!({"file": "a.rs", "symbol": "Foo/bar", "column": 3}));
        assert!(err.message.contains("'column' applies only with 'line'"));
        assert!(err.hint.unwrap().contains("Drop 'column'"));
    }
}
