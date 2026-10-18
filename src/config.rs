//! LSP Runtime Configuration

use std::collections::BTreeMap;
use std::time::Duration;

use crate::models::config::SymoraConfig;
use crate::models::symbol::Language;

#[derive(Debug, Clone)]
pub struct LspRuntimeConfig {
    base_timeout: Duration,
    pub max_file_size_bytes: u64,
    pub auto_restart: bool,
    /// Wait window for `publishDiagnostics` after a document sync.
    /// Confirmation short-circuits; the window only runs out when the
    /// server stays silent.
    pub diagnostics_wait: Duration,
    /// Custom project entry files per language (overrides defaults in find_project_entry)
    pub entry_files: BTreeMap<String, Vec<String>>,
    /// [lsp.servers] launch overrides, keyed by Language::lsp_id(). Merged
    /// over servers::defaults() by LspManager at construction. Only
    /// validated (canonical-key) entries reach this map.
    pub servers: BTreeMap<String, crate::models::config::ServerOverride>,
    /// Hard cap on simultaneously running language servers. Prevents
    /// memory-bound monorepos from spawning a server per language and
    /// running out of file descriptors / RAM.
    pub max_concurrent_servers: usize,
}

impl Default for LspRuntimeConfig {
    fn default() -> Self {
        Self {
            base_timeout: Duration::from_secs(60),
            max_file_size_bytes: 10 * 1024 * 1024,
            auto_restart: true,
            diagnostics_wait: Duration::from_millis(
                crate::models::config::defaults::diagnostics_wait_ms(),
            ),
            entry_files: BTreeMap::new(),
            servers: BTreeMap::new(),
            max_concurrent_servers: crate::constants::defaults::LSP_MAX_CONCURRENT_SERVERS,
        }
    }
}

impl From<&SymoraConfig> for LspRuntimeConfig {
    fn from(config: &SymoraConfig) -> Self {
        Self {
            base_timeout: Duration::from_secs(config.lsp.timeout_secs),
            max_file_size_bytes: config.search.max_file_size_bytes(),
            auto_restart: config.lsp.auto_restart,
            diagnostics_wait: Duration::from_millis(config.lsp.diagnostics_wait_ms),
            entry_files: config.project.entry_files.clone(),
            servers: config.lsp.servers.clone(),
            max_concurrent_servers: crate::constants::defaults::LSP_MAX_CONCURRENT_SERVERS,
        }
    }
}

impl LspRuntimeConfig {
    pub fn request_timeout(&self) -> Duration {
        self.base_timeout
    }

    pub fn indexing_wait(&self, language: Language) -> Duration {
        match language {
            Language::Rust => Duration::from_secs(8),
            Language::Java => Duration::from_secs(10),
            _ => Duration::ZERO,
        }
    }

    pub fn entry_files_for(&self, language: Language) -> Option<&Vec<String>> {
        self.entry_files.get(language.lsp_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_budget_uses_the_configured_duration() {
        let mut config = SymoraConfig::default();
        config.lsp.timeout_secs = 7;
        assert_eq!(
            LspRuntimeConfig::from(&config).request_timeout(),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn runtime_config_carries_server_overrides() {
        use crate::models::config::ServerOverride;

        let mut config = SymoraConfig::default();
        config.lsp.servers.insert(
            "rust".to_string(),
            ServerOverride {
                command: Some("/custom/rust-analyzer".to_string()),
                args: None,
                tier: None,
                env: None,
            },
        );
        let runtime = LspRuntimeConfig::from(&config);
        assert_eq!(runtime.servers, config.lsp.servers);
        assert!(LspRuntimeConfig::default().servers.is_empty());
    }
}
