use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::{OutputContext, OutputOptions};
use crate::config::LspRuntimeConfig;
use crate::models::config::SymoraConfig;
use crate::services::ast_query::{AstQueryService, DefaultAstQueryService};
use crate::services::config::{ConfigService, DefaultConfigService};
#[cfg(unix)]
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
    test_matcher: crate::cli::utils::TestMatcher,
}

impl App {
    pub async fn new(output_options: OutputOptions, use_daemon: bool) -> anyhow::Result<Self> {
        let root = std::env::current_dir()?;

        tracing::debug!("Initializing Symora at {:?}", root);

        let output = OutputContext::new(root.clone(), output_options);
        let config_service = Arc::new(DefaultConfigService::new(&root));
        let config = config_service.load(false).await.unwrap_or_default();

        let runtime_config = Arc::new(LspRuntimeConfig::from(&config));

        let project = Arc::new(DefaultProjectService::new(&root));
        let ast = Arc::new(DefaultAstQueryService::new(
            runtime_config.max_file_size_bytes,
        )?);

        #[cfg(unix)]
        let lsp: Arc<dyn LspService + Send + Sync> = if use_daemon {
            Arc::new(DaemonLspService::new(&root))
        } else {
            Arc::new(DefaultLspService::new(&root, Arc::clone(&runtime_config)))
        };

        #[cfg(not(unix))]
        let lsp: Arc<dyn LspService + Send + Sync> = {
            let _ = use_daemon;
            Arc::new(DefaultLspService::new(&root, Arc::clone(&runtime_config)))
        };

        tracing::info!(
            "Symora initialized (daemon: {})",
            if use_daemon { "enabled" } else { "disabled" }
        );

        let test_matcher = crate::cli::utils::TestMatcher::from_config(&config.test);

        Ok(Self {
            root,
            output,
            lsp,
            ast,
            project,
            config_service,
            config,
            test_matcher,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &SymoraConfig {
        &self.config
    }

    pub fn test_matcher(&self) -> &crate::cli::utils::TestMatcher {
        &self.test_matcher
    }
}
