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

use crate::daemon::proves_no_listener;
use crate::infra::file_lock::FileLock;
use tokio::sync::{RwLock, Semaphore, watch};

pub use config::DaemonRuntimeConfig;
use connection::handle_connection;
use context::{ProjectContext, ProjectsMap};

pub struct DaemonServer {
    config: Arc<DaemonRuntimeConfig>,
    lsp_config: Arc<crate::config::LspRuntimeConfig>,
    projects: ProjectsMap,
    semaphore: Arc<Semaphore>,
    start_time: Instant,
    /// Level-triggered, so the signal is a state rather than an event: an
    /// observer that subscribes after the flag is already set still sees it
    /// (`wait_for` checks the current value before waiting). Serving must
    /// stop everywhere at once — the socket is released when the accept loop
    /// breaks, and a replacement daemon binds it — so an observer that
    /// subscribes a moment late must not be the one connection that keeps
    /// answering for projects the replacement now owns.
    shutdown: watch::Sender<bool>,
}

impl DaemonServer {
    pub fn new(
        config: DaemonRuntimeConfig,
        lsp_config: Arc<crate::config::LspRuntimeConfig>,
    ) -> Self {
        let (shutdown, _) = watch::channel(false);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent));
        Self {
            config: Arc::new(config),
            lsp_config,
            semaphore,
            projects: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
            shutdown,
        }
    }

    /// Bind the daemon socket, refusing to displace a daemon that holds
    /// it. The socket file is a claim, not a fact: a path nobody answers on
    /// is the leftover of a process that died, and is replaced. Probing and
    /// binding happen under one lock, so a live listener — or a server
    /// mid-claim — keeps the path and this process exits rather than leave
    /// two daemons behind, one of them unreachable.
    async fn claim_socket(&self) -> Result<UnixListener, std::io::Error> {
        let already_serving = || {
            std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "a daemon is already listening on {}",
                    self.config.socket_path.display()
                ),
            )
        };

        let Some(_claim) = FileLock::exclusive(&self.config.bind_lock_path)? else {
            return Err(already_serving());
        };
        match tokio::net::UnixStream::connect(&self.config.socket_path).await {
            Ok(_) => return Err(already_serving()),
            // Only a refused or absent socket is a leftover to replace;
            // unlinking one this process merely could not reach would
            // strand the daemon still bound to it.
            Err(e) if !proves_no_listener(&e) => return Err(e),
            Err(_) => {}
        }
        let _ = tokio::fs::remove_file(&self.config.socket_path).await;
        UnixListener::bind(&self.config.socket_path)
    }

    pub async fn run(&self) -> Result<(), std::io::Error> {
        self.setup_socket_dir().await?;

        let listener = self.claim_socket().await?;

        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&self.config.socket_path, perms).await?;
        }

        tracing::info!("Daemon listening on {:?}", self.config.socket_path);
        self.write_pid_file().await?;

        let mut shutdown_rx = self.shutdown.subscribe();
        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(60));
        cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Ordered: shutdown ends serving as early as it can be seen,
            // then the cleanup tick, then acceptance. Cleanup is ready once a
            // minute and returns, so putting it ahead of `accept` costs an
            // arriving connection nothing; the reverse order would let a
            // permanently ready accept queue starve idle-server eviction for
            // as long as the load lasts.
            tokio::select! {
                biased;
                _ = shutdown_rx.wait_for(|stopping| *stopping) => {
                    tracing::info!("Shutdown signal received");
                    break;
                }
                _ = cleanup_interval.tick() => {
                    self.cleanup_idle_servers().await;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => self.spawn_connection_handler(stream),
                        Err(e) => tracing::warn!("Failed to accept connection: {}", e),
                    }
                }
            }
        }

        // Serving ends here, so the socket is released here — before the
        // teardown below, which checkpoints every project's store and shuts
        // down its language servers and can take many times longer than the
        // client's wait. A bound listener answers connections whether or not
        // anything is being served, so holding it across teardown makes a
        // daemon that has stopped look like one that never did, and a
        // successful shutdown gets reported as a timeout. Nothing here
        // unlinks the path: the next daemon settles it under the bind lock,
        // once it has confirmed nobody answers.
        drop(listener);
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
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                stream, projects, semaphore, config, lsp_config, start_time, shutdown,
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
            let cache_expired = ctx.lsp.cleanup_expired_caches().await;
            if cache_expired > 0 {
                tracing::debug!("Cleaned up {} expired cache entries", cache_expired);
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

    /// Flush what this daemon owns exclusively. The socket and pid files
    /// are claims the NEXT daemon settles when it binds, never removed
    /// here: a teardown slow enough to overlap a successor would otherwise
    /// delete the successor's socket and strand it.
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
    }

    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }
}
