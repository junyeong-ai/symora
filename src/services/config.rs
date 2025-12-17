//! Configuration service for Symora

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;

use crate::error::ConfigError;
use crate::models::config::SymoraConfig;

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

    async fn load_from_path(path: &Path) -> Result<SymoraConfig, ConfigError> {
        if !path.exists() {
            return Ok(SymoraConfig::default());
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

#[async_trait]
impl ConfigService for DefaultConfigService {
    async fn load(&self, global_only: bool) -> Result<SymoraConfig, ConfigError> {
        if global_only {
            return Self::load_from_path(&Self::global_config_path()).await;
        }

        let mut config = Self::load_from_path(&Self::global_config_path()).await?;
        let project_config = Self::load_from_path(&self.project_config_path()).await?;
        config = merge_config(config, project_config);
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

fn merge_config(base: SymoraConfig, overlay: SymoraConfig) -> SymoraConfig {
    SymoraConfig {
        project: if overlay.project.name.is_some() {
            overlay.project
        } else {
            crate::models::config::ProjectConfig {
                name: base.project.name.or(overlay.project.name),
                languages: if overlay.project.languages.is_empty() {
                    base.project.languages
                } else {
                    overlay.project.languages
                },
                ignored_paths: overlay.project.ignored_paths,
            }
        },
        lsp: overlay.lsp,
        search: overlay.search,
        output: overlay.output,
        daemon: base.daemon, // daemon settings from global only
    }
}

fn apply_env_overrides(mut config: SymoraConfig) -> SymoraConfig {
    if let Ok(val) = std::env::var("SYMORA_OUTPUT_FORMAT") {
        config.output.format = val;
    }
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
