//! LSP Runtime Configuration

use std::collections::BTreeMap;
use std::time::Duration;

use crate::models::config::SymoraConfig;
use crate::models::symbol::Language;

#[derive(Debug, Clone, Copy)]
pub struct LanguageProfile {
    pub timeout_multiplier: f64,
    pub indexing_wait_ms: u64,
    pub cross_file_wait_ms: u64,
    pub aggressive_retry: bool,
}

impl LanguageProfile {
    pub const fn new(
        timeout_multiplier: f64,
        indexing_wait_ms: u64,
        cross_file_wait_ms: u64,
        aggressive_retry: bool,
    ) -> Self {
        Self {
            timeout_multiplier,
            indexing_wait_ms,
            cross_file_wait_ms,
            aggressive_retry,
        }
    }

    pub fn for_language(language: Language) -> Self {
        match language {
            // Kotlin-LS: Pre-alpha, needs extended time for Gradle/Maven project indexing
            Language::Kotlin => Self::new(10.0, 15000, 2000, true),
            // Pyright: Large monorepo projects need significant indexing time
            Language::Python => Self::new(8.0, 30000, 1500, true),
            // JDT.LS: Large project indexing with Gradle/Maven dependency resolution
            Language::Java => Self::new(3.0, 10000, 1000, true),
            // TypeScript/JavaScript: Monorepo with thousands of files needs extended indexing
            // Cross-file wait helps with tsserver's lazy indexing behavior
            Language::TypeScript | Language::JavaScript => Self::new(2.5, 15000, 1000, false),
            // rust-analyzer: Cargo workspace indexing
            Language::Rust => Self::new(1.5, 8000, 0, false),
            // gopls: Fast but module graph can be large
            Language::Go => Self::new(1.0, 5000, 0, false),
            // clangd: compile_commands.json parsing
            Language::Cpp => Self::new(1.5, 5000, 500, false),
            // csharp-ls/OmniSharp: Solution parsing
            Language::CSharp => Self::new(2.0, 8000, 500, false),
            _ => Self::new(1.5, 3000, 0, false),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    Request,
    WorkspaceOperation,
    Rename,
    Initialization,
    Shutdown,
}

impl OperationType {
    pub fn from_method(method: &str) -> Self {
        match method {
            "textDocument/rename" | "textDocument/prepareRename" => Self::Rename,
            "workspace/symbol"
            | "textDocument/references"
            | "textDocument/implementation"
            | "textDocument/prepareCallHierarchy"
            | "callHierarchy/incomingCalls"
            | "callHierarchy/outgoingCalls" => Self::WorkspaceOperation,
            "initialize" => Self::Initialization,
            "shutdown" => Self::Shutdown,
            _ => Self::Request,
        }
    }

    fn base_multiplier(self) -> f64 {
        match self {
            Self::Request => 1.0,
            Self::WorkspaceOperation => 6.0,
            Self::Rename => 10.0,
            Self::Initialization => 2.0,
            Self::Shutdown => 0.5,
        }
    }
}

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
            max_file_size_bytes: u64::from(config.search.max_file_size_mb) * 1024 * 1024,
            auto_restart: config.lsp.auto_restart,
            diagnostics_wait: Duration::from_millis(config.lsp.diagnostics_wait_ms),
            entry_files: config.project.entry_files.clone(),
            servers: config.lsp.servers.clone(),
            max_concurrent_servers: crate::constants::defaults::LSP_MAX_CONCURRENT_SERVERS,
        }
    }
}

impl LspRuntimeConfig {
    pub fn timeout_for(&self, language: Language, method: &str) -> Duration {
        let profile = LanguageProfile::for_language(language);
        let op_type = OperationType::from_method(method);
        let multiplier = profile.timeout_multiplier * op_type.base_multiplier();
        Duration::from_secs_f64(self.base_timeout.as_secs_f64() * multiplier)
    }

    pub fn indexing_wait(&self, language: Language) -> Duration {
        Duration::from_millis(LanguageProfile::for_language(language).indexing_wait_ms)
    }

    pub fn cross_file_wait(&self, language: Language) -> Duration {
        Duration::from_millis(LanguageProfile::for_language(language).cross_file_wait_ms)
    }

    pub fn entry_files_for(&self, language: Language) -> Option<&Vec<String>> {
        self.entry_files.get(language.lsp_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_profile_kotlin() {
        let profile = LanguageProfile::for_language(Language::Kotlin);
        assert_eq!(profile.timeout_multiplier, 10.0);
        assert_eq!(profile.cross_file_wait_ms, 2000);
        assert!(profile.aggressive_retry);
    }

    #[test]
    fn test_cross_file_wait() {
        let config = LspRuntimeConfig::default();

        // TypeScript: 1000ms
        assert_eq!(
            config.cross_file_wait(Language::TypeScript),
            Duration::from_millis(1000)
        );

        // Rust: 0ms (fast indexing)
        assert_eq!(
            config.cross_file_wait(Language::Rust),
            Duration::from_millis(0)
        );

        // Python: 1500ms
        assert_eq!(
            config.cross_file_wait(Language::Python),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn test_timeout_calculation() {
        let config = LspRuntimeConfig::default();

        // Rust normal request: 60s * 1.5 * 1.0 = 90s
        let rust_timeout = config.timeout_for(Language::Rust, "textDocument/hover");
        assert_eq!(rust_timeout, Duration::from_secs(90));

        // Kotlin workspace operation: 60s * 10.0 * 6.0 = 3600s
        let kotlin_timeout = config.timeout_for(Language::Kotlin, "workspace/symbol");
        assert_eq!(kotlin_timeout, Duration::from_secs(3600));

        // Python initialization: 60s * 8.0 * 2.0 = 960s
        let python_timeout = config.timeout_for(Language::Python, "initialize");
        assert_eq!(python_timeout, Duration::from_secs(960));

        // TypeScript rename: 60s * 2.5 * 10.0 = 1500s
        let ts_rename = config.timeout_for(Language::TypeScript, "textDocument/rename");
        assert_eq!(ts_rename, Duration::from_secs(1500));
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
            },
        );
        let runtime = LspRuntimeConfig::from(&config);
        assert_eq!(runtime.servers, config.lsp.servers);
        assert!(LspRuntimeConfig::default().servers.is_empty());
    }

    #[test]
    fn test_operation_type_parsing() {
        assert_eq!(
            OperationType::from_method("textDocument/hover"),
            OperationType::Request
        );
        assert_eq!(
            OperationType::from_method("workspace/symbol"),
            OperationType::WorkspaceOperation
        );
        assert_eq!(
            OperationType::from_method("textDocument/rename"),
            OperationType::Rename
        );
        assert_eq!(
            OperationType::from_method("textDocument/prepareRename"),
            OperationType::Rename
        );
        assert_eq!(
            OperationType::from_method("initialize"),
            OperationType::Initialization
        );
        assert_eq!(
            OperationType::from_method("shutdown"),
            OperationType::Shutdown
        );
    }
}
