use std::collections::BTreeMap;
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

    async fn load_raw_from_path(path: &Path) -> Result<RawParse, ConfigError> {
        if !path.exists() {
            return Ok(RawParse::default());
        }
        let content = tokio::fs::read_to_string(path).await?;
        parse_raw(&content, path)
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
    fn load_raw_sync(path: &Path) -> Result<RawParse, ConfigError> {
        if !path.exists() {
            return Ok(RawParse::default());
        }
        let content = std::fs::read_to_string(path)?;
        parse_raw(&content, path)
    }

    if global_only {
        let raw = load_raw_sync(&DefaultConfigService::global_config_path())?;
        return Ok(resolve_config(raw.config, raw.unknown_keys));
    }

    let service = DefaultConfigService::new(root);
    let global = load_raw_sync(&DefaultConfigService::global_config_path())?;
    let project = load_raw_sync(&service.project_config_path())?;
    let unknown_keys = [global.unknown_keys, project.unknown_keys].concat();
    let merged = merge_raw_config(global.config, project.config);
    let mut config = resolve_config(merged, unknown_keys);
    config = apply_env_overrides(config);
    Ok(config)
}

#[async_trait]
impl ConfigService for DefaultConfigService {
    async fn load(&self, global_only: bool) -> Result<SymoraConfig, ConfigError> {
        if global_only {
            let raw = Self::load_raw_from_path(&Self::global_config_path()).await?;
            return Ok(resolve_config(raw.config, raw.unknown_keys));
        }

        let global = Self::load_raw_from_path(&Self::global_config_path()).await?;
        let project = Self::load_raw_from_path(&self.project_config_path()).await?;
        let unknown_keys = [global.unknown_keys, project.unknown_keys].concat();
        let merged = merge_raw_config(global.config, project.config);
        let mut config = resolve_config(merged, unknown_keys);
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

/// A parsed config file plus the keys nothing in it consumed.
#[derive(Debug, Clone, Default)]
struct RawParse {
    config: RawSymoraConfig,
    unknown_keys: Vec<String>,
}

/// Deserialize a config file, recording every key the typed shape ignored.
///
/// Serde drops unknown keys silently, which turns a mistyped or retired
/// setting into one that quietly does nothing — the failure rejected
/// `[lsp.servers]` stanzas are already reported to avoid, generalised to
/// every section. Refusing the whole file instead would discard the
/// settings that are correct, so the keys are collected and disclosed while
/// the rest applies.
fn parse_raw(content: &str, path: &Path) -> Result<RawParse, ConfigError> {
    let mut unknown_keys = Vec::new();
    let deserializer = toml::Deserializer::parse(content)
        .map_err(|e| ConfigError::Parse(format!("{}: {e}", path.display())))?;
    let config: RawSymoraConfig = serde_ignored::deserialize(deserializer, |key| {
        unknown_keys.push(format!("{}: unknown key `{key}`", path.display()));
    })
    .map_err(|e| ConfigError::Parse(format!("{}: {e}", path.display())))?;
    Ok(RawParse {
        config,
        unknown_keys,
    })
}

#[derive(Debug, Clone, Deserialize, Default)]
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
    /// Raw [lsp.servers.<lang>] stanzas. Kept as TOML tables so
    /// `resolve_server_overrides` can partition unknown keys AND unknown
    /// fields into corrective errors instead of failing the whole config
    /// or silently dropping a typo'd field. Ordered by key so the
    /// resolve — and the `config_errors` it reports — reads the stanzas
    /// in one canonical order.
    #[serde(default)]
    servers: BTreeMap<String, toml::Table>,
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
    }
}

// ---------------------------------------------------------------------------
// Resolve: apply defaults to any fields that were never explicitly set
// ---------------------------------------------------------------------------

fn resolve_config(raw: RawSymoraConfig, unknown_keys: Vec<String>) -> SymoraConfig {
    use crate::models::config::defaults;
    use crate::models::config::*;

    let (servers, server_override_errors) = resolve_server_overrides(raw.lsp.servers);

    SymoraConfig {
        unknown_keys,
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

/// The field set a [lsp.servers.<lang>] stanza accepts — the
/// `ServerOverride` fields, kept in lockstep by the unit test below.
const SERVER_OVERRIDE_FIELDS: [&str; 3] = ["command", "args", "tier"];

/// Partition [lsp.servers] stanzas: strictly canonical `Language::lsp_id`
/// keys with only known, well-typed fields are applied; alias keys,
/// unknown keys, unknown fields, and mistyped values are recorded with a
/// corrective message and the stanza dropped — never applied, never a
/// load error for the rest of the config. A stanza's unknown fields and
/// mistyped values are reported together in one pass, and the stanza is
/// rejected whole: applying its remainder would let a typo'd `command`
/// silently fall back to the builtin launch.
fn resolve_server_overrides(
    raw: BTreeMap<String, toml::Table>,
) -> (BTreeMap<String, ServerOverride>, Vec<ServerOverrideError>) {
    let mut applied = BTreeMap::new();
    let mut errors = Vec::new();
    for (key, table) in raw {
        match Language::from_str(&key) {
            Ok(language) if language.lsp_id() == key => {
                let mut known = toml::Table::new();
                let mut stanza_errors = Vec::new();
                for (field, value) in table {
                    if SERVER_OVERRIDE_FIELDS.contains(&field.as_str()) {
                        known.insert(field, value);
                    } else {
                        stanza_errors.push(ServerOverrideError {
                            key: format!("lsp.servers.{key}.{field}"),
                            message: format!(
                                "unknown field `{field}` — valid fields are `{}`",
                                SERVER_OVERRIDE_FIELDS.join("`, `")
                            ),
                        });
                    }
                }
                // Typing the known-field remainder surfaces mistyped
                // values alongside any unknown fields in the same load.
                match toml::Value::Table(known).try_into::<ServerOverride>() {
                    Ok(value) => {
                        if stanza_errors.is_empty() {
                            applied.insert(key, value);
                        }
                    }
                    Err(e) => stanza_errors.push(ServerOverrideError {
                        key: format!("lsp.servers.{key}"),
                        message: single_line(&e.to_string()),
                    }),
                }
                errors.extend(stanza_errors);
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

/// toml's error messages span multiple lines (message, then `in \`field\``
/// context); a recorded config error travels inside a JSON string, so
/// collapse internal whitespace runs to single spaces.
fn single_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

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
        resolve_config(raw, Vec::new())
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
    fn server_override_unknown_field_recorded_not_applied() {
        let config = resolve_str(
            r#"
[lsp]
timeout_secs = 99

[lsp.servers.rust]
comand = "/custom/rust-analyzer"
"#,
        );
        assert!(config.lsp.servers.is_empty());
        assert_eq!(config.lsp.timeout_secs, 99);
        let errors = &config.lsp.server_override_errors;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].key, "lsp.servers.rust.comand");
        assert!(errors[0].message.contains("unknown field `comand`"));
        for field in SERVER_OVERRIDE_FIELDS {
            assert!(errors[0].message.contains(&format!("`{field}`")));
        }
    }

    #[test]
    fn server_override_mistyped_value_recorded_not_applied() {
        let config = resolve_str("[lsp.servers.rust]\nargs = \"--stdio\"\n");
        assert!(config.lsp.servers.is_empty());
        let errors = &config.lsp.server_override_errors;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].key, "lsp.servers.rust");
        // toml's multi-line message is collapsed: the error travels inside
        // a JSON string, where embedded newlines read as literal `\n`.
        assert!(!errors[0].message.is_empty());
        assert!(!errors[0].message.contains('\n'));
    }

    /// Both error classes for one stanza surface in a single load — an
    /// unknown field must not pre-empt the mistyped-value report, or the
    /// user fixes the typo only to hit a second rejection.
    #[test]
    fn server_override_reports_unknown_field_and_mistyped_value_together() {
        let config = resolve_str("[lsp.servers.rust]\ncomand = \"/x\"\nargs = \"--stdio\"\n");
        assert!(config.lsp.servers.is_empty());
        let errors = &config.lsp.server_override_errors;
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].key, "lsp.servers.rust");
        assert!(errors[0].message.contains("invalid type"));
        assert_eq!(errors[1].key, "lsp.servers.rust.comand");
        assert!(errors[1].message.contains("unknown field `comand`"));
    }

    /// A stanza whose known fields are all well-typed is still rejected
    /// whole when it carries an unknown field.
    #[test]
    fn server_override_unknown_field_still_rejects_well_typed_remainder() {
        let config = resolve_str("[lsp.servers.rust]\ncommand = \"/x\"\nbogus = 1\n");
        assert!(config.lsp.servers.is_empty());
        let errors = &config.lsp.server_override_errors;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].key, "lsp.servers.rust.bogus");
    }

    /// `SERVER_OVERRIDE_FIELDS` is the unknown-field gate; it must track
    /// the `ServerOverride` struct exactly or a real field would be
    /// rejected (or a removed one accepted).
    #[test]
    fn server_override_field_list_stays_in_lockstep() {
        let full = ServerOverride {
            command: Some("/x".to_string()),
            args: Some(vec![]),
            tier: Some(ServerTier::Fast),
        };
        let table = toml::Value::try_from(&full).unwrap();
        let mut keys: Vec<&str> = table
            .as_table()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected = SERVER_OVERRIDE_FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
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
        let resolved = resolve_config(merge_raw_config(global, project), Vec::new());
        let o = &resolved.lsp.servers["rust"];
        assert_eq!(o.command.as_deref(), Some("/project/rust-analyzer"));
        // Wholesale replacement: the global stanza's explicit args are gone.
        assert_eq!(o.args, None);
    }
}
