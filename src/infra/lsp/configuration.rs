use std::path::Path;

use serde_json::{Value, json};

use crate::config::LspRuntimeConfig;
use crate::models::symbol::Language;

use super::init_options::init_options;

pub struct ServerConfiguration {
    pub initialization_options: Option<Value>,
    pub settings: Value,
}

impl ServerConfiguration {
    pub fn resolve(language: Language, root: &Path, config: &LspRuntimeConfig) -> Self {
        let mut initialization_options = init_options(language, root);
        let mut settings = initialization_options.clone().unwrap_or_else(|| json!({}));
        if let Some(overrides) = config.servers.get(language.lsp_id()) {
            if let Some(options) = &overrides.initialization_options {
                merge(
                    initialization_options.get_or_insert_with(|| json!({})),
                    Value::Object(options.clone()),
                );
            }
            if let Some(overrides) = &overrides.settings {
                merge(&mut settings, Value::Object(overrides.clone()));
            }
        }
        Self {
            initialization_options,
            settings,
        }
    }
}

fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(base) => merge(base, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_lsp_channels_preserve_defaults_and_explicit_values() {
        let root = tempfile::tempdir().unwrap();
        let mut config = LspRuntimeConfig::default();
        config.servers.insert(
            "python".into(),
            crate::models::config::ServerOverride {
                initialization_options: Some(
                    json!({"python": {"analysis": {"logLevel": "Trace"}}})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                settings: Some(
                    json!({"python": {"analysis": {
                        "autoSearchPaths": false,
                        "extraPaths": ["packages/core/src"],
                        "exclude": []
                    }}})
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
                ..Default::default()
            },
        );
        let resolved = ServerConfiguration::resolve(Language::Python, root.path(), &config);
        let options = resolved.initialization_options.unwrap();
        assert_eq!(options["python"]["analysis"]["logLevel"], "Trace");
        assert_eq!(options["python"]["analysis"]["autoSearchPaths"], true);
        assert_eq!(
            resolved.settings["python"]["analysis"]["logLevel"],
            "Information"
        );
        assert_eq!(
            resolved.settings["python"]["analysis"]["autoSearchPaths"],
            false
        );
        assert_eq!(
            resolved.settings["python"]["analysis"]["exclude"],
            json!([])
        );
        assert_eq!(
            resolved.settings["python"]["analysis"]["extraPaths"],
            json!(["packages/core/src"])
        );
        assert_eq!(
            resolved.settings["python"]["analysis"]["useLibraryCodeForTypes"],
            true
        );
        let rust = ServerConfiguration::resolve(Language::Rust, root.path(), &config);
        assert!(rust.settings.get("python").is_none());
    }

    #[test]
    fn explicit_options_work_without_builtin_options() {
        let root = tempfile::tempdir().unwrap();
        let mut config = LspRuntimeConfig::default();
        assert!(
            ServerConfiguration::resolve(Language::Cpp, root.path(), &config)
                .initialization_options
                .is_none()
        );
        config.servers.insert(
            "cpp".into(),
            crate::models::config::ServerOverride {
                initialization_options: Some(
                    json!({"fallbackFlags": ["-std=c++20"]})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                settings: Some(
                    json!({"clangd": {"trace": "verbose"}})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                ..Default::default()
            },
        );
        let resolved = ServerConfiguration::resolve(Language::Cpp, root.path(), &config);
        assert_eq!(
            resolved.initialization_options,
            Some(json!({"fallbackFlags": ["-std=c++20"]}))
        );
        assert_eq!(resolved.settings, json!({"clangd": {"trace": "verbose"}}));
    }
}
