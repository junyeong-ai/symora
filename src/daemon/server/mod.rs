mod config;
mod connection;
mod context;
mod dispatch;
mod handlers;
mod store_handlers;

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, Semaphore, broadcast};

pub use config::DaemonRuntimeConfig;
use connection::handle_connection;
use context::{ProjectContext, ProjectsMap};

pub struct DaemonServer {
    config: Arc<DaemonRuntimeConfig>,
    lsp_config: Arc<crate::config::LspRuntimeConfig>,
    projects: ProjectsMap,
    semaphore: Arc<Semaphore>,
    start_time: Instant,
    shutdown_tx: broadcast::Sender<()>,
}

impl DaemonServer {
    pub fn new(
        config: DaemonRuntimeConfig,
        lsp_config: Arc<crate::config::LspRuntimeConfig>,
    ) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent));
        Self {
            config: Arc::new(config),
            lsp_config,
            semaphore,
            projects: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
            shutdown_tx,
        }
    }

    pub async fn run(&self) -> Result<(), std::io::Error> {
        self.setup_socket_dir().await?;

        let _ = tokio::fs::remove_file(&self.config.socket_path).await;
        let listener = UnixListener::bind(&self.config.socket_path)?;

        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&self.config.socket_path, perms).await?;
        }

        tracing::info!("Daemon listening on {:?}", self.config.socket_path);
        self.write_pid_file().await?;

        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(60));
        cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => self.spawn_connection_handler(stream),
                        Err(e) => tracing::warn!("Failed to accept connection: {}", e),
                    }
                }
                _ = cleanup_interval.tick() => {
                    self.cleanup_idle_servers().await;
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Shutdown signal received");
                    break;
                }
            }
        }

        self.cleanup().await;
        Ok(())
    }

    async fn setup_socket_dir(&self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.config.socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            {
                let perms = std::fs::Permissions::from_mode(0o700);
                tokio::fs::set_permissions(parent, perms).await?;
            }
        }
        Ok(())
    }

    fn spawn_connection_handler(&self, stream: UnixStream) {
        let projects = Arc::clone(&self.projects);
        let semaphore = Arc::clone(&self.semaphore);
        let config = Arc::clone(&self.config);
        let lsp_config = Arc::clone(&self.lsp_config);
        let start_time = self.start_time;
        let shutdown_tx = self.shutdown_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                stream,
                projects,
                semaphore,
                config,
                lsp_config,
                start_time,
                shutdown_tx,
            )
            .await
            {
                tracing::warn!("Connection error: {}", e);
            }
        });
    }

    async fn write_pid_file(&self) -> Result<(), std::io::Error> {
        tokio::fs::write(&self.config.pid_path, std::process::id().to_string()).await?;
        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&self.config.pid_path, perms).await?;
        }
        Ok(())
    }

    async fn cleanup_idle_servers(&self) {
        let (contexts, idle_paths): (Vec<(PathBuf, Arc<ProjectContext>)>, Vec<PathBuf>) = {
            let projects = self.projects.read().await;
            let contexts = projects
                .iter()
                .map(|(p, c)| (p.clone(), Arc::clone(c)))
                .collect();
            let idle = projects
                .iter()
                .filter(|(_, ctx)| ctx.is_idle(self.config.idle_timeout))
                .map(|(p, _)| p.clone())
                .collect();
            (contexts, idle)
        };

        for (_, ctx) in &contexts {
            let expired = ctx.store.cleanup_expired().await;
            let cache_expired = ctx.lsp.cleanup_expired_caches().await;
            if expired + cache_expired > 0 {
                tracing::debug!(
                    "Cleaned up {} expired store entries and {} expired cache entries",
                    expired,
                    cache_expired
                );
            }
        }

        if idle_paths.is_empty() {
            return;
        }

        let idle: Vec<_> = {
            let mut projects = self.projects.write().await;
            let still_idle: Vec<_> = idle_paths
                .into_iter()
                .filter(|path| {
                    projects
                        .get(path)
                        .is_some_and(|ctx| ctx.is_idle(self.config.idle_timeout))
                })
                .collect();
            still_idle
                .into_iter()
                .filter_map(|path| projects.remove(&path).map(|ctx| (path, ctx)))
                .collect()
        };

        for (path, ctx) in idle {
            if let Err(e) = ctx.store.checkpoint().await {
                tracing::debug!("Failed to checkpoint store for {:?}: {}", path, e);
            }
            ctx.lsp.shutdown().await;
            tracing::info!("Removed idle project: {:?}", path);
        }
    }

    async fn cleanup(&self) {
        let contexts: Vec<_> = {
            let projects = self.projects.read().await;
            projects
                .iter()
                .map(|(p, c)| (p.clone(), Arc::clone(c)))
                .collect()
        };

        for (_path, ctx) in &contexts {
            if let Err(e) = ctx.store.checkpoint().await {
                tracing::debug!("Failed to checkpoint store during shutdown: {}", e);
            }
            ctx.lsp.shutdown().await;
        }

        if let Err(e) = tokio::fs::remove_file(&self.config.socket_path).await {
            tracing::debug!("Failed to remove socket: {}", e);
        }
        if let Err(e) = tokio::fs::remove_file(&self.config.pid_path).await {
            tracing::debug!("Failed to remove pid file: {}", e);
        }
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }
}
