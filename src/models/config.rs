//! Configuration model for Symora
//!
//! Simple configuration focused on LSP-first architecture.

use serde::{Deserialize, Serialize};

use super::symbol::Language;

/// Symora configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SymoraConfig {
    #[serde(default)]
    pub project: ProjectConfig,

    #[serde(default)]
    pub lsp: LspConfig,

    #[serde(default)]
    pub search: SearchConfig,

    #[serde(default)]
    pub output: OutputConfig,

    #[serde(default)]
    pub daemon: DaemonSettings,
}

/// Project configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Project name
    pub name: Option<String>,

    /// Detected/configured languages
    #[serde(default)]
    pub languages: Vec<Language>,

    /// Paths to ignore
    #[serde(default = "default_ignored_paths")]
    pub ignored_paths: Vec<String>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            name: None,
            languages: Vec::new(),
            ignored_paths: default_ignored_paths(),
        }
    }
}

fn default_ignored_paths() -> Vec<String> {
    vec![
        "node_modules".to_string(),
        ".git".to_string(),
        "target".to_string(),
        "dist".to_string(),
        "build".to_string(),
        "__pycache__".to_string(),
        ".venv".to_string(),
        "venv".to_string(),
        ".symora".to_string(),
    ]
}

/// LSP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspConfig {
    #[serde(default = "defaults::timeout_secs")]
    pub timeout_secs: u64,

    #[serde(default = "defaults::auto_restart")]
    pub auto_restart: bool,

    #[serde(default = "defaults::refs_limit")]
    pub refs_limit: usize,

    #[serde(default = "defaults::impl_limit")]
    pub impl_limit: usize,

    #[serde(default = "defaults::symbol_limit")]
    pub symbol_limit: usize,

    #[serde(default = "defaults::calls_limit")]
    pub calls_limit: usize,

    #[serde(default)]
    pub servers: LspServerCommands,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            timeout_secs: defaults::timeout_secs(),
            auto_restart: defaults::auto_restart(),
            refs_limit: defaults::refs_limit(),
            impl_limit: defaults::impl_limit(),
            symbol_limit: defaults::symbol_limit(),
            calls_limit: defaults::calls_limit(),
            servers: LspServerCommands::default(),
        }
    }
}

mod defaults {
    // LSP
    pub fn timeout_secs() -> u64 {
        30
    }
    pub fn auto_restart() -> bool {
        true
    }
    pub fn refs_limit() -> usize {
        500
    }
    pub fn impl_limit() -> usize {
        100
    }
    pub fn symbol_limit() -> usize {
        100
    }
    pub fn calls_limit() -> usize {
        100
    }

    // Search
    pub fn search_limit() -> usize {
        100
    }
    pub fn max_file_size_mb() -> u32 {
        5
    }

    // Output
    pub fn format() -> String {
        "json".to_string()
    }
    pub fn color() -> bool {
        true
    }

    // Daemon
    pub fn max_concurrent() -> usize {
        100
    }
    pub fn idle_timeout_mins() -> u64 {
        30
    }
}

/// LSP server commands per language
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LspServerCommands {
    #[serde(default)]
    pub kotlin: Option<String>,

    #[serde(default)]
    pub rust: Option<String>,

    #[serde(default)]
    pub typescript: Option<String>,

    #[serde(default)]
    pub python: Option<String>,

    #[serde(default)]
    pub go: Option<String>,

    #[serde(default)]
    pub java: Option<String>,
}

/// Search configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "defaults::search_limit")]
    pub limit: usize,

    #[serde(default = "defaults::max_file_size_mb")]
    pub max_file_size_mb: u32,

    #[serde(default)]
    pub ripgrep_path: Option<String>,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            limit: defaults::search_limit(),
            max_file_size_mb: defaults::max_file_size_mb(),
            ripgrep_path: None,
        }
    }
}

impl SearchConfig {
    pub fn max_file_size_bytes(&self) -> u64 {
        if self.max_file_size_mb == 0 {
            u64::MAX
        } else {
            self.max_file_size_mb as u64 * 1024 * 1024
        }
    }
}

/// Output configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(default = "defaults::format")]
    pub format: String,

    #[serde(default = "defaults::color")]
    pub color: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: defaults::format(),
            color: defaults::color(),
        }
    }
}

/// Daemon settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonSettings {
    #[serde(default = "defaults::max_concurrent")]
    pub max_concurrent: usize,

    #[serde(default = "defaults::idle_timeout_mins")]
    pub idle_timeout_mins: u64,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            max_concurrent: defaults::max_concurrent(),
            idle_timeout_mins: defaults::idle_timeout_mins(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = SymoraConfig::default();
        assert_eq!(config.lsp.timeout_secs, 30);
        assert_eq!(config.lsp.refs_limit, 500);
        assert_eq!(config.lsp.calls_limit, 100);
        assert_eq!(config.search.limit, 100);
        assert_eq!(config.output.format, "json");
        assert_eq!(config.daemon.idle_timeout_mins, 30);
    }

    #[test]
    fn test_ignored_paths() {
        let config = SymoraConfig::default();
        assert!(
            config
                .project
                .ignored_paths
                .contains(&".symora".to_string())
        );
        assert!(
            config
                .project
                .ignored_paths
                .contains(&"node_modules".to_string())
        );
    }
}
