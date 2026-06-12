use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::ConfigError;
use crate::models::config::{ServerOverride, ServerOverrideError, SymoraConfig};
use crate::models::symbol::Language;

#[async_trait]
pub trait ConfigService: Send + Sync {
    async fn load(&self, global_only: bool) -> Result<SymoraConfig, ConfigError>;
    fn config_path(&self, global: bool) -> PathBuf;
    async fn init(&self, global: bool, force: bool) -> Result<PathBuf, ConfigError>;
    async fn edit(&self, global: bool) -> Result<PathBuf, ConfigError>;
}

pub struct DefaultConfigService {
    root: PathBuf,
}

impl DefaultConfigService {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    fn global_config_path() -> PathBuf {
        // XDG standard: ~/.config/symora/config.toml
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .ok()
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("symora")
            .join("config.toml")
    }

    fn project_config_path(&self) -> PathBuf {
        self.root.join(".symora").join("config.toml")
    }

    async fn load_raw_from_path(path: &Path) -> Result<RawSymoraConfig, ConfigError> {
        if !path.exists() {
            return Ok(RawSymoraConfig::default());
        }
        let content = tokio::fs::read_to_string(path).await?;
        toml::from_str(&content).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    async fn write_default_config(path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let config = SymoraConfig::default();
        let content =
            toml::to_string_pretty(&config).map_err(|e| ConfigError::Parse(e.to_string()))?;
        tokio::fs::write(path, content).await?;
        Ok(())
    }

    fn get_editor() -> String {
        std::env::var("EDITOR").unwrap_or_else(|_| {
            if cfg!(target_os = "macos") {
                "open".to_string()
            } else if cfg!(target_os = "windows") {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        })
    }
}

pub fn load_merged_config_sync(
    root: &Path,
    global_only: bool,
) -> Result<SymoraConfig, ConfigError> {
    fn load_raw_sync(path: &Path) -> Result<RawSymoraConfig, ConfigError> {
        if !path.exists() {
            return Ok(RawSymoraConfig::default());
        }
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    if global_only {
        let raw = load_raw_sync(&DefaultConfigService::global_config_path())?;
        return Ok(resolve_config(raw));
    }

    let service = DefaultConfigService::new(root);
    let global = load_raw_sync(&DefaultConfigService::global_config_path())?;
    let project = load_raw_sync(&service.project_config_path())?;
    let merged = merge_raw_config(global, project);
    let mut config = resolve_config(merged);
    config = apply_env_overrides(config);
    Ok(config)
}

#[async_trait]
impl ConfigService for DefaultConfigService {
    async fn load(&self, global_only: bool) -> Result<SymoraConfig, ConfigError> {
        if global_only {
            let raw = Self::load_raw_from_path(&Self::global_config_path()).await?;
            return Ok(resolve_config(raw));
        }

        let global = Self::load_raw_from_path(&Self::global_config_path()).await?;
        let project = Self::load_raw_from_path(&self.project_config_path()).await?;
        let merged = merge_raw_config(global, project);
        let mut config = resolve_config(merged);
        config = apply_env_overrides(config);
        Ok(config)
    }

    fn config_path(&self, global: bool) -> PathBuf {
        if global {
            Self::global_config_path()
        } else {
            self.project_config_path()
        }
    }

    async fn init(&self, global: bool, force: bool) -> Result<PathBuf, ConfigError> {
        let path = self.config_path(global);

        if path.exists() && !force {
            return Err(ConfigError::InvalidValue {
                key: "config".to_string(),
                message: format!(
                    "Config already exists: {}. Use --force to overwrite.",
                    path.display()
                ),
            });
        }

        Self::write_default_config(&path).await?;
        Ok(path)
    }

    async fn edit(&self, global: bool) -> Result<PathBuf, ConfigError> {
        let path = self.config_path(global);

        if !path.exists() {
            return Err(ConfigError::NotFound(format!(
                "Config file does not exist: {}\nRun: symora config init{}",
                path.display(),
                if global { " --global" } else { "" }
            )));
        }

        let editor = Self::get_editor();
        let status =
            Command::new(&editor)
                .arg(&path)
                .status()
                .map_err(|e| ConfigError::InvalidValue {
                    key: "editor".to_string(),
                    message: format!("Failed to launch editor '{}': {}", editor, e),
                })?;

        if !status.success() {
            return Err(ConfigError::InvalidValue {
                key: "editor".to_string(),
                message: "Editor exited with error".to_string(),
            });
        }

        Ok(path)
    }
}

// ---------------------------------------------------------------------------
// Raw config types for TOML parsing — Option fields track explicit settings
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct RawSymoraConfig {
    #[serde(default)]
    project: crate::models::config::ProjectConfig,
    #[serde(default)]
    lsp: RawLspConfig,
    #[serde(default)]
    search: RawSearchConfig,
    #[serde(default)]
    daemon: RawDaemonConfig,
    #[serde(default)]
    output: RawOutputConfig,
    #[serde(default)]
    test: crate::models::config::TestConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawLspConfig {
    timeout_secs: Option<u64>,
    auto_restart: Option<bool>,
    refs_limit: Option<usize>,
    impl_limit: Option<usize>,
    symbol_limit: Option<usize>,
    calls_limit: Option<usize>,
    type_hierarchy_limit: Option<usize>,
    tests_limit: Option<usize>,
    diagnostics_wait_ms: Option<u64>,
    #[serde(default)]
    servers: HashMap<String, ServerOverride>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawSearchConfig {
    limit: Option<usize>,
    max_file_size_mb: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawDaemonConfig {
    max_concurrent: Option<usize>,
    idle_timeout_mins: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawOutputConfig {
    max_response_chars: Option<usize>,
}

// ---------------------------------------------------------------------------
// Merge raw configs: overlay.field.or(base.field) preserves explicit values
// ---------------------------------------------------------------------------

fn merge_raw_config(base: RawSymoraConfig, overlay: RawSymoraConfig) -> RawSymoraConfig {
    RawSymoraConfig {
        project: merge_project(base.project, overlay.project),
        lsp: merge_raw_lsp(base.lsp, overlay.lsp),
        search: merge_raw_search(base.search, overlay.search),
        daemon: merge_raw_daemon(base.daemon, overlay.daemon),
        output: merge_raw_output(base.output, overlay.output),
        test: merge_test(base.test, overlay.test),
    }
}

fn merge_project(
    base: crate::models::config::ProjectConfig,
    overlay: crate::models::config::ProjectConfig,
) -> crate::models::config::ProjectConfig {
    crate::models::config::ProjectConfig {
        name: overlay.name.or(base.name),
        languages: if overlay.languages.is_empty() {
            base.languages
        } else {
            overlay.languages
        },
        ignored_paths: merge_vec(base.ignored_paths, overlay.ignored_paths),
        entry_files: {
            let mut merged = base.entry_files;
            merged.extend(overlay.entry_files);
            merged
        },
    }
}

fn merge_raw_lsp(base: RawLspConfig, overlay: RawLspConfig) -> RawLspConfig {
    RawLspConfig {
        timeout_secs: overlay.timeout_secs.or(base.timeout_secs),
        auto_restart: overlay.auto_restart.or(base.auto_restart),
        refs_limit: overlay.refs_limit.or(base.refs_limit),
        impl_limit: overlay.impl_limit.or(base.impl_limit),
        symbol_limit: overlay.symbol_limit.or(base.symbol_limit),
        calls_limit: overlay.calls_limit.or(base.calls_limit),
        type_hierarchy_limit: overlay.type_hierarchy_limit.or(base.type_hierarchy_limit),
        tests_limit: overlay.tests_limit.or(base.tests_limit),
        diagnostics_wait_ms: overlay.diagnostics_wait_ms.or(base.diagnostics_wait_ms),
        servers: {
            let mut merged = base.servers;
            merged.extend(overlay.servers);
            merged
        },
    }
}

fn merge_raw_search(base: RawSearchConfig, overlay: RawSearchConfig) -> RawSearchConfig {
    RawSearchConfig {
        limit: overlay.limit.or(base.limit),
        max_file_size_mb: overlay.max_file_size_mb.or(base.max_file_size_mb),
    }
}

fn merge_raw_daemon(base: RawDaemonConfig, overlay: RawDaemonConfig) -> RawDaemonConfig {
    RawDaemonConfig {
        max_concurrent: overlay.max_concurrent.or(base.max_concurrent),
        idle_timeout_mins: overlay.idle_timeout_mins.or(base.idle_timeout_mins),
    }
}

fn merge_raw_output(base: RawOutputConfig, overlay: RawOutputConfig) -> RawOutputConfig {
    RawOutputConfig {
        max_response_chars: overlay.max_response_chars.or(base.max_response_chars),
    }
}

fn merge_test(
    base: crate::models::config::TestConfig,
    overlay: crate::models::config::TestConfig,
) -> crate::models::config::TestConfig {
    crate::models::config::TestConfig {
        file_patterns: merge_vec(base.file_patterns, overlay.file_patterns),
        dir_patterns: merge_vec(base.dir_patterns, overlay.dir_patterns),
        markers: merge_vec(base.markers, overlay.markers),
    }
}

// ---------------------------------------------------------------------------
// Resolve: apply defaults to any fields that were never explicitly set
// ---------------------------------------------------------------------------

fn resolve_config(raw: RawSymoraConfig) -> SymoraConfig {
    use crate::models::config::defaults;
    use crate::models::config::*;

    let (servers, server_override_errors) = resolve_server_overrides(raw.lsp.servers);

    SymoraConfig {
        project: raw.project,
        lsp: LspConfig {
            timeout_secs: raw.lsp.timeout_secs.unwrap_or_else(defaults::timeout_secs),
            auto_restart: raw.lsp.auto_restart.unwrap_or_else(defaults::auto_restart),
            refs_limit: raw.lsp.refs_limit.unwrap_or_else(defaults::refs_limit),
            impl_limit: raw.lsp.impl_limit.unwrap_or_else(defaults::impl_limit),
            symbol_limit: raw.lsp.symbol_limit.unwrap_or_else(defaults::symbol_limit),
            calls_limit: raw.lsp.calls_limit.unwrap_or_else(defaults::calls_limit),
            type_hierarchy_limit: raw
                .lsp
                .type_hierarchy_limit
                .unwrap_or_else(defaults::type_hierarchy_limit),
            tests_limit: raw.lsp.tests_limit.unwrap_or_else(defaults::tests_limit),
            diagnostics_wait_ms: raw
                .lsp
                .diagnostics_wait_ms
                .unwrap_or_else(defaults::diagnostics_wait_ms),
            servers,
            server_override_errors,
        },
        search: SearchConfig {
            limit: raw.search.limit.unwrap_or_else(defaults::search_limit),
            max_file_size_mb: raw
                .search
                .max_file_size_mb
                .unwrap_or_else(defaults::max_file_size_mb),
        },
        daemon: DaemonConfig {
            max_concurrent: raw
                .daemon
                .max_concurrent
                .unwrap_or_else(defaults::max_concurrent),
            idle_timeout_mins: raw
                .daemon
                .idle_timeout_mins
                .unwrap_or_else(defaults::idle_timeout_mins),
        },
        output: OutputConfig {
            max_response_chars: raw
                .output
                .max_response_chars
                .unwrap_or_else(defaults::max_response_chars),
        },
        test: raw.test,
    }
}

/// Partition [lsp.servers] keys: strictly canonical `Language::lsp_id`
/// keys are applied; alias and unknown keys are recorded with a
/// corrective message and dropped — never applied, never a load error
/// for the rest of the config.
fn resolve_server_overrides(
    raw: HashMap<String, ServerOverride>,
) -> (HashMap<String, ServerOverride>, Vec<ServerOverrideError>) {
    let mut applied = HashMap::new();
    let mut errors = Vec::new();
    for (key, value) in raw {
        match Language::from_str(&key) {
            Ok(language) if language.lsp_id() == key => {
                applied.insert(key, value);
            }
            Ok(language) => errors.push(ServerOverrideError {
                key: format!("lsp.servers.{key}"),
                message: format!(
                    "use `{}` (the canonical language id shown by `symora doctor`)",
                    language.lsp_id()
                ),
            }),
            Err(_) => errors.push(ServerOverrideError {
                key: format!("lsp.servers.{key}"),
                message: "unknown language — use the `language` id shown by `symora doctor`"
                    .to_string(),
            }),
        }
    }
    errors.sort_by(|a, b| a.key.cmp(&b.key));
    (applied, errors)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn merge_vec(base: Vec<String>, overlay: Vec<String>) -> Vec<String> {
    if overlay.is_empty() {
        base
    } else {
        let mut merged = base;
        for item in overlay {
            if !merged.contains(&item) {
                merged.push(item);
            }
        }
        merged
    }
}

fn apply_env_overrides(mut config: SymoraConfig) -> SymoraConfig {
    if let Ok(val) = std::env::var("SYMORA_SEARCH_LIMIT")
        && let Ok(limit) = val.parse()
    {
        config.search.limit = limit;
    }
    if let Ok(val) = std::env::var("SYMORA_LSP_TIMEOUT")
        && let Ok(timeout) = val.parse()
    {
        config.lsp.timeout_secs = timeout;
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::config::ServerTier;

    fn resolve_str(content: &str) -> SymoraConfig {
        let raw: RawSymoraConfig = toml::from_str(content).unwrap();
        resolve_config(raw)
    }

    #[test]
    fn server_override_parses_and_resolves() {
        let config = resolve_str(
            r#"
[lsp.servers.typescript]
command = "/custom/typescript-language-server"
args = ["--stdio"]
tier = "slow"
"#,
        );
        let o = &config.lsp.servers["typescript"];
        assert_eq!(
            o.command.as_deref(),
            Some("/custom/typescript-language-server")
        );
        assert_eq!(o.args, Some(vec!["--stdio".to_string()]));
        assert_eq!(o.tier, Some(ServerTier::Slow));
        assert!(config.lsp.server_override_errors.is_empty());
    }

    #[test]
    fn server_override_unknown_key_recorded_not_applied() {
        let config = resolve_str("[lsp.servers.klingon]\ncommand = \"/nope\"\n");
        assert!(config.lsp.servers.is_empty());
        assert_eq!(config.lsp.server_override_errors.len(), 1);
        assert_eq!(
            config.lsp.server_override_errors[0].key,
            "lsp.servers.klingon"
        );
    }

    #[test]
    fn server_override_alias_key_suggests_canonical() {
        let config = resolve_str(
            "[lsp.servers.ts]\ncommand = \"/a\"\n\n[lsp.servers.bash]\ncommand = \"/b\"\n",
        );
        assert!(config.lsp.servers.is_empty());
        let errors = &config.lsp.server_override_errors;
        assert_eq!(errors.len(), 2);
        let ts = errors.iter().find(|e| e.key == "lsp.servers.ts").unwrap();
        assert!(ts.message.contains("typescript"));
        let bash = errors.iter().find(|e| e.key == "lsp.servers.bash").unwrap();
        assert!(bash.message.contains("shellscript"));
    }

    #[test]
    fn server_override_rejection_preserves_rest_of_config() {
        let config = resolve_str(
            r#"
[lsp]
timeout_secs = 99

[lsp.servers.klingon]
command = "/nope"

[lsp.servers.rust]
command = "/custom/rust-analyzer"
"#,
        );
        assert_eq!(config.lsp.timeout_secs, 99);
        assert_eq!(
            config.lsp.servers["rust"].command.as_deref(),
            Some("/custom/rust-analyzer")
        );
        assert_eq!(config.lsp.server_override_errors.len(), 1);
        assert_eq!(
            config.lsp.server_override_errors[0].key,
            "lsp.servers.klingon"
        );
    }

    #[test]
    fn project_server_override_replaces_global() {
        let global: RawSymoraConfig =
            toml::from_str("[lsp.servers.rust]\ncommand = \"/global/rust-analyzer\"\nargs = []\n")
                .unwrap();
        let project: RawSymoraConfig =
            toml::from_str("[lsp.servers.rust]\ncommand = \"/project/rust-analyzer\"\n").unwrap();
        let resolved = resolve_config(merge_raw_config(global, project));
        let o = &resolved.lsp.servers["rust"];
        assert_eq!(o.command.as_deref(), Some("/project/rust-analyzer"));
        // Wholesale replacement: the global stanza's explicit args are gone.
        assert_eq!(o.args, None);
    }
}
