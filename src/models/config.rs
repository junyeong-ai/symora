use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::symbol::Language;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SymoraConfig {
    #[serde(default)]
    pub project: ProjectConfig,

    #[serde(default)]
    pub lsp: LspConfig,

    #[serde(default)]
    pub search: SearchConfig,

    #[serde(default)]
    pub daemon: DaemonConfig,

    #[serde(default)]
    pub output: OutputConfig,

    #[serde(default)]
    pub test: TestConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: Option<String>,

    #[serde(default)]
    pub languages: Vec<Language>,

    #[serde(default)]
    pub entry_files: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    #[serde(default = "defaults::type_hierarchy_limit")]
    pub type_hierarchy_limit: usize,

    #[serde(default = "defaults::tests_limit")]
    pub tests_limit: usize,

    /// How long to wait for the server to publish diagnostics for the
    /// synced content. Confirmation returns immediately; the window only
    /// runs out when the server stays silent, in which case results are
    /// reported as unconfirmed rather than synthesized as clean.
    #[serde(default = "defaults::diagnostics_wait_ms")]
    pub diagnostics_wait_ms: u64,

    /// [lsp.servers.<lang>] launch overrides, keyed by Language::lsp_id().
    /// Only validated (canonical-key) entries live here.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub servers: std::collections::HashMap<String, ServerOverride>,

    /// Rejected [lsp.servers] stanzas from the last resolve (non-canonical
    /// keys, unknown fields, mistyped values) — never applied, never
    /// serialized. Disclosed by `symora doctor` as `config_errors`.
    #[serde(skip)]
    pub server_override_errors: Vec<ServerOverrideError>,
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
            type_hierarchy_limit: defaults::type_hierarchy_limit(),
            tests_limit: defaults::tests_limit(),
            diagnostics_wait_ms: defaults::diagnostics_wait_ms(),
            servers: std::collections::HashMap::new(),
            server_override_errors: Vec::new(),
        }
    }
}

/// A [lsp.servers.<lang>] launch override. An absent field inherits the
/// builtin default for that server; an explicit `args = []` means no args.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<ServerTier>,
}

/// A rejected [lsp.servers] stanza or field, recorded at config
/// resolution. Never serialized; carried so `doctor` can disclose
/// overrides that did not apply without re-parsing config. Display
/// matches ConfigError::InvalidValue: "Invalid value for '{key}':
/// {message}".
#[derive(Debug, Clone, PartialEq)]
pub struct ServerOverrideError {
    pub key: String,
    pub message: String,
}

impl fmt::Display for ServerOverrideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid value for '{}': {}", self.key, self.message)
    }
}

pub(crate) mod defaults {
    pub fn timeout_secs() -> u64 {
        60
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
    pub fn type_hierarchy_limit() -> usize {
        100
    }
    pub fn tests_limit() -> usize {
        10
    }
    pub fn diagnostics_wait_ms() -> u64 {
        1500
    }

    // Search
    pub fn search_limit() -> usize {
        100
    }
    pub fn max_file_size_mb() -> u32 {
        5
    }

    // Daemon
    pub fn max_concurrent() -> usize {
        100
    }
    pub fn idle_timeout_mins() -> u64 {
        30
    }

    // Output
    pub fn max_response_chars() -> usize {
        20_000
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerTier {
    /// Fast servers (< 15s init): rust-analyzer, clangd, gopls
    Fast,
    /// Standard servers (15-45s init): intelephense, kotlin-ls, ruby-lsp
    Standard,
    /// Slow servers (45-120s init): pyright, typescript-language-server, jdtls
    Slow,
}

impl ServerTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Standard => "standard",
            Self::Slow => "slow",
        }
    }

    pub fn init_timeout(&self) -> Duration {
        match self {
            Self::Fast => Duration::from_secs(15),
            Self::Standard => Duration::from_secs(45),
            Self::Slow => Duration::from_secs(120),
        }
    }

    pub fn request_timeout(&self) -> Duration {
        match self {
            Self::Fast => Duration::from_secs(15),
            Self::Standard => Duration::from_secs(30),
            Self::Slow => Duration::from_secs(60),
        }
    }

    pub fn cross_file_timeout(&self) -> Duration {
        match self {
            Self::Fast => Duration::from_secs(20),
            Self::Standard => Duration::from_secs(45),
            Self::Slow => Duration::from_secs(90),
        }
    }

    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "defaults::search_limit")]
    pub limit: usize,

    #[serde(default = "defaults::max_file_size_mb")]
    pub max_file_size_mb: u32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            limit: defaults::search_limit(),
            max_file_size_mb: defaults::max_file_size_mb(),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "defaults::max_concurrent")]
    pub max_concurrent: usize,

    #[serde(default = "defaults::idle_timeout_mins")]
    pub idle_timeout_mins: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            max_concurrent: defaults::max_concurrent(),
            idle_timeout_mins: defaults::idle_timeout_mins(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputConfig {
    /// Char ceiling on each emitted JSON response, measured on the exact
    /// serialized string in the active format. When a response exceeds it,
    /// Section items are dropped whole (never reshaped) until it fits; the
    /// reduction is disclosed via truncated + a hint. 0 disables.
    #[serde(default = "defaults::max_response_chars")]
    pub max_response_chars: usize,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            max_response_chars: defaults::max_response_chars(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestConfig {
    #[serde(default)]
    pub file_patterns: Vec<String>,

    #[serde(default)]
    pub dir_patterns: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = SymoraConfig::default();
        assert_eq!(config.lsp.timeout_secs, 60);
        assert_eq!(config.lsp.refs_limit, 500);
        assert_eq!(config.lsp.calls_limit, 100);
        assert_eq!(config.lsp.tests_limit, 10);
        assert_eq!(config.search.limit, 100);
        assert_eq!(config.daemon.idle_timeout_mins, 30);
        assert_eq!(config.output.max_response_chars, 20_000);
    }
}
