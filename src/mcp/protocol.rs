use serde::Serialize;
use serde_json::Value;

/// Spec-standard tool behaviour hints. Always emitted: a stable presence
/// rule beats clients guessing what an absent hint means (the spec default
/// for `readOnlyHint` is `false`, i.e. "may modify").
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ToolAnnotations {
    #[serde(rename = "readOnlyHint")]
    pub read_only_hint: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub annotations: ToolAnnotations,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(rename = "outputSchema", skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

impl ToolDefinition {
    /// A tool that never writes source files.
    pub fn read_only(name: &'static str, description: &'static str, input_schema: Value) -> Self {
        Self::build(name, description, input_schema, true)
    }

    /// A tool that writes source files. Its description must carry the
    /// literal word `Mutates` — a catalog test enforces the pairing, so
    /// the human-readable warning and the typed hint can't drift.
    pub fn mutating(name: &'static str, description: &'static str, input_schema: Value) -> Self {
        Self::build(name, description, input_schema, false)
    }

    fn build(
        name: &'static str,
        description: &'static str,
        input_schema: Value,
        read_only_hint: bool,
    ) -> Self {
        Self {
            name,
            description,
            annotations: ToolAnnotations { read_only_hint },
            input_schema,
            output_schema: None,
        }
    }

    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }
}

impl From<&ToolDefinition> for rmcp::model::Tool {
    fn from(definition: &ToolDefinition) -> Self {
        let mut tool = Self::default();
        tool.name = definition.name.into();
        tool.description = Some(definition.description.into());
        tool.input_schema = std::sync::Arc::new(
            definition
                .input_schema
                .as_object()
                .expect("tool input schema is an object")
                .clone(),
        );
        tool.output_schema = definition.output_schema.as_ref().map(|schema| {
            std::sync::Arc::new(
                schema
                    .as_object()
                    .expect("tool output schema is an object")
                    .clone(),
            )
        });
        let mut annotations = rmcp::model::ToolAnnotations::default();
        annotations.read_only_hint = Some(definition.annotations.read_only_hint);
        tool.annotations = Some(annotations);
        tool
    }
}
