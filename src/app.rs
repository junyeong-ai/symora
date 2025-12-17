//! Application container for Symora

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::OutputContext;
use crate::config;
use crate::models::config::SymoraConfig;
use crate::services::ast_query::{AstQueryService, DefaultAstQueryService};
use crate::services::config::{ConfigService, DefaultConfigService};
use crate::services::daemon_lsp::DaemonLspService;
use crate::services::lsp::{DefaultLspService, LspService};
use crate::services::project::{DefaultProjectService, ProjectService};

pub struct App {
    root: PathBuf,
    pub(crate) output: OutputContext,
    pub(crate) lsp: Arc<dyn LspService + Send + Sync>,
    pub(crate) ast: Arc<dyn AstQueryService>,
    pub(crate) project: Arc<dyn ProjectService>,
    pub(crate) config_service: Arc<dyn ConfigService>,
    pub(crate) config: SymoraConfig,
    daemon_mode: bool,
}

impl App {
    pub async fn new() -> anyhow::Result<Self> {
        Self::with_daemon(true).await
    }

    pub async fn with_daemon(use_daemon: bool) -> anyhow::Result<Self> {
        let root = std::env::current_dir()?;

        tracing::debug!("Initializing Symora at {:?}", root);

        let output = OutputContext::new(root.clone());
        let config_service = Arc::new(DefaultConfigService::new(&root));
        let config = config_service.load(false).await.unwrap_or_default();

        // Initialize global config singleton (thread-safe, no unsafe)
        config::init(&config);

        let project = Arc::new(DefaultProjectService::new(&root));
        let ast = Arc::new(DefaultAstQueryService::new()?);

        let lsp: Arc<dyn LspService + Send + Sync> = if use_daemon {
            Arc::new(DaemonLspService::new(&root))
        } else {
            Arc::new(DefaultLspService::new(&root))
        };

        tracing::info!(
            "Symora initialized (daemon: {})",
            if use_daemon { "enabled" } else { "disabled" }
        );

        Ok(Self {
            root,
            output,
            lsp,
            ast,
            project,
            config_service,
            config,
            daemon_mode: use_daemon,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &SymoraConfig {
        &self.config
    }

    pub fn is_daemon_mode(&self) -> bool {
        self.daemon_mode
    }

    pub fn is_initialized(&self) -> bool {
        self.root.join(".symora").exists()
    }
}
