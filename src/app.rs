use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::output::OutputSink;
use crate::cli::{OutputContext, OutputOptions};
use crate::config::LspRuntimeConfig;
use crate::models::config::SymoraConfig;
use crate::services::ast_query::{AstQueryService, DefaultAstQueryService};
use crate::services::config::{ConfigService, DefaultConfigService};
#[cfg(unix)]
use crate::services::daemon_lsp::DaemonLspService;
#[cfg(unix)]
use crate::services::daemon_store::DaemonStoreService;
use crate::services::lsp::{DefaultLspService, LspService};
use crate::services::project::{DefaultProjectService, ProjectService};
use crate::services::store::{DefaultStoreService, StoreConfig, StoreService};

pub struct App {
    root: PathBuf,
    pub(crate) output: OutputContext,
    pub(crate) lsp: Arc<dyn LspService + Send + Sync>,
    pub(crate) ast: Arc<dyn AstQueryService>,
    pub(crate) project: Arc<dyn ProjectService>,
    pub(crate) store: Arc<dyn StoreService>,
    pub(crate) config_service: Arc<dyn ConfigService>,
    pub(crate) config: SymoraConfig,
    test_matcher: crate::cli::utils::TestMatcher,
}

impl App {
    pub async fn new(output_options: OutputOptions, use_daemon: bool) -> anyhow::Result<Self> {
        Self::new_at(std::env::current_dir()?, output_options, use_daemon).await
    }

    /// Build an `App` rooted at an explicit path. `new` is this seeded with
    /// the current directory; rooting elsewhere lets a caller drive a
    /// specific tree without mutating process-global cwd.
    pub(crate) async fn new_at(
        root: std::path::PathBuf,
        output_options: OutputOptions,
        use_daemon: bool,
    ) -> anyhow::Result<Self> {
        tracing::debug!("Initializing Symora at {:?}", root);

        let config_service = Arc::new(DefaultConfigService::new(&root));
        // A whole-config load failure (malformed `.symora/config.toml`, I/O
        // error) must not vanish: the run proceeds on defaults, but the failure
        // is captured and surfaced on whatever command the user ran, never left
        // visible only to `doctor`. Rejected `[lsp.servers]` stanzas are a
        // softer class already disclosed by doctor / config show, so they are
        // not duplicated here.
        let (config, config_errors) = match config_service.load(false).await {
            Ok(config) => (config, Vec::new()),
            Err(e) => {
                tracing::warn!("config load failed, falling back to defaults: {e}");
                (SymoraConfig::default(), vec![e.to_string()])
            }
        };

        let output = OutputContext::new(root.clone(), output_options)
            .with_max_response_chars(config.output.max_response_chars)
            .with_config_errors(config_errors);

        let runtime_config = Arc::new(LspRuntimeConfig::from(&config));

        let project = Arc::new(DefaultProjectService::new(&root));
        let ast = Arc::new(DefaultAstQueryService::new(
            runtime_config.max_file_size_bytes,
        ));

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

        #[cfg(unix)]
        let store: Arc<dyn StoreService> = if use_daemon {
            Arc::new(DaemonStoreService::new(&root))
        } else {
            Arc::new(DefaultStoreService::new(&root, StoreConfig::default()))
        };

        #[cfg(not(unix))]
        let store: Arc<dyn StoreService> =
            Arc::new(DefaultStoreService::new(&root, StoreConfig::default()));

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
            store,
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

    /// True once a handler reported a handled failure via `print_error`.
    /// Exposed publicly so the binary crate's `main` can set a non-zero exit
    /// code (the field on `OutputContext` is `pub(crate)`).
    pub fn errored(&self) -> bool {
        self.output.errored()
    }

    /// Return a clone of `self` with the output sink replaced.
    ///
    /// Used by the MCP adapter to capture command output into a buffer
    /// instead of stdout, without spawning a subprocess.
    pub fn with_output_sink(&self, sink: Arc<dyn OutputSink>, options: OutputOptions) -> Self {
        let output = OutputContext::with_sink(self.root.clone(), options, sink)
            .with_max_response_chars(self.config.output.max_response_chars)
            .with_config_errors(self.output.config_errors_snapshot());
        Self {
            root: self.root.clone(),
            output,
            lsp: Arc::clone(&self.lsp),
            ast: Arc::clone(&self.ast),
            project: Arc::clone(&self.project),
            store: Arc::clone(&self.store),
            config_service: Arc::clone(&self.config_service),
            config: self.config.clone(),
            test_matcher: self.test_matcher.clone(),
        }
    }
}
