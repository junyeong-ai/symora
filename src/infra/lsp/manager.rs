use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, RwLock};

use super::client::LspClient;
use super::servers::{self, ServerConfig};
use crate::error::LspError;
use crate::models::symbol::Language;

enum ClientState {
    Initializing(Arc<Notify>),
    Live {
        client: Arc<LspClient>,
        last_used: Instant,
    },
}

impl ClientState {
    fn live(client: Arc<LspClient>) -> Self {
        Self::Live {
            client,
            last_used: Instant::now(),
        }
    }

    fn touch(&mut self) {
        if let Self::Live { last_used, .. } = self {
            *last_used = Instant::now();
        }
    }

    fn idle_duration(&self) -> Duration {
        match self {
            Self::Live { last_used, .. } => last_used.elapsed(),
            Self::Initializing(_) => Duration::ZERO,
        }
    }

    fn client(&self) -> Option<Arc<LspClient>> {
        match self {
            Self::Live { client, .. } => Some(Arc::clone(client)),
            Self::Initializing(_) => None,
        }
    }
}

pub struct LspManager {
    root: PathBuf,
    clients: RwLock<HashMap<Language, ClientState>>,
    configs: HashMap<Language, ServerConfig>,
    runtime_config: Arc<crate::config::LspRuntimeConfig>,
}

impl LspManager {
    pub fn new(root: PathBuf, runtime_config: Arc<crate::config::LspRuntimeConfig>) -> Self {
        Self {
            root,
            clients: RwLock::new(HashMap::new()),
            configs: servers::merged(&runtime_config.servers),
            runtime_config,
        }
    }

    /// Get or start a client for a language (race-safe, deadlock-free)
    pub async fn get_client(&self, language: Language) -> Result<Arc<LspClient>, LspError> {
        loop {
            // Phase 1: Get client or notify under lock, release immediately
            let (client_opt, notify_opt) = {
                let clients = self.clients.read().await;
                match clients.get(&language) {
                    Some(ClientState::Live { client, .. }) => (Some(Arc::clone(client)), None),
                    Some(ClientState::Initializing(notify)) => (None, Some(Arc::clone(notify))),
                    None => (None, None),
                }
            };

            // Phase 2: Check if running outside lock
            if let Some(client) = client_opt
                && client.is_running().await
            {
                let mut clients = self.clients.write().await;
                if let Some(state) = clients.get_mut(&language) {
                    state.touch();
                }
                return Ok(client);
            }
            // Dead client - need to restart

            // Phase 3: Wait for initialization or start new
            if let Some(notify) = notify_opt {
                notify.notified().await;
                continue;
            }

            // Phase 4: Start new client (with LRU eviction if pool full)
            let notify = Arc::new(Notify::new());
            let evict = {
                let mut clients = self.clients.write().await;
                if clients.contains_key(&language) {
                    continue; // Race: another thread started, retry
                }
                let cap = self.runtime_config.max_concurrent_servers.max(1);
                let evict = self.pick_eviction_target(&clients);
                if clients.len() >= cap && evict.is_none() {
                    // At capacity with nothing evictable: every occupant
                    // is mid-startup. Wait for one to settle instead of
                    // exceeding the cap with another Initializing entry.
                    let waiter = clients.values().find_map(|state| match state {
                        ClientState::Initializing(n) => Some(Arc::clone(n)),
                        _ => None,
                    });
                    drop(clients);
                    if let Some(waiter) = waiter {
                        // Timeout guards the registration race between
                        // releasing the lock and polling the Notified
                        // future; the loop re-checks either way.
                        let _ = tokio::time::timeout(Duration::from_millis(250), waiter.notified())
                            .await;
                    }
                    continue;
                }
                clients.insert(language, ClientState::Initializing(Arc::clone(&notify)));
                evict
            };

            if let Some(victim) = evict
                && let Err(e) = self.shutdown_client(victim).await
            {
                tracing::warn!(
                    "Failed to evict {:?} before starting {:?}: {}",
                    victim,
                    language,
                    e
                );
            }

            return self.start_client_internal(language, notify).await;
        }
    }

    /// Pick the least-recently-used Ready client when the pool is full.
    /// Returns `None` when there's still headroom under
    /// `max_concurrent_servers`.
    ///
    /// Deliberately NOT keyed on `IndexingState`: `Initializing` is
    /// already immune (the `_ => None` arm below), and `InProgress` is
    /// self-bounding — `await_indexing_signal` races an unconditional sleep,
    /// so a hung server transitions to `TimedOut` and becomes evictable
    /// on its own. Adding an `InProgress` immunity would make a full pool
    /// of indexing clients unevictable. When the pool is at capacity and
    /// every occupant is `Initializing` (this returns `None`), `get_client`
    /// waits for one to settle rather than exceeding the cap.
    fn pick_eviction_target(&self, clients: &HashMap<Language, ClientState>) -> Option<Language> {
        let cap = self.runtime_config.max_concurrent_servers.max(1);
        if clients.len() < cap {
            return None;
        }
        clients
            .iter()
            .filter_map(|(lang, state)| match state {
                ClientState::Live { last_used, .. } => Some((*lang, *last_used)),
                _ => None,
            })
            .min_by_key(|(_, last_used)| *last_used)
            .map(|(lang, _)| lang)
    }

    async fn start_client_internal(
        &self,
        language: Language,
        notify: Arc<Notify>,
    ) -> Result<Arc<LspClient>, LspError> {
        let result = self.do_start_client(language).await;

        let mut clients = self.clients.write().await;
        match &result {
            Ok(client) => {
                clients.insert(language, ClientState::live(Arc::clone(client)));
            }
            Err(_) => {
                clients.remove(&language);
            }
        }
        notify.notify_waiters();

        result
    }

    async fn do_start_client(&self, language: Language) -> Result<Arc<LspClient>, LspError> {
        let config = self
            .configs
            .get(&language)
            .ok_or_else(|| LspError::UnsupportedLanguage(format!("{:?}", language)))?;

        // Resolution is the only install gate: an executable we can
        // resolve is "installed", and spawning it is the truth test.
        let command = config.resolve()?;

        let client = LspClient::new(
            language,
            self.root.clone(),
            Arc::clone(&self.runtime_config),
        );
        client
            .start(&command.to_string_lossy(), &config.args)
            .await?;

        tracing::info!("{:?} language server started", language);
        Ok(client)
    }

    pub async fn shutdown_client(&self, language: Language) -> Result<(), LspError> {
        let client = {
            let mut clients = self.clients.write().await;
            clients.remove(&language).and_then(|s| s.client())
        };

        if let Some(client) = client {
            client.shutdown().await?;
            tracing::info!("{:?} language server stopped", language);
        }

        Ok(())
    }

    pub async fn restart_client(&self, language: Language) -> Result<Arc<LspClient>, LspError> {
        if let Err(e) = self.shutdown_client(language).await {
            tracing::warn!("Error shutting down {:?} before restart: {}", language, e);
        }
        tracing::info!("{:?} language server restarting", language);
        self.get_client(language).await
    }

    pub async fn shutdown_all(&self) {
        let clients_to_shutdown: Vec<(Language, Arc<LspClient>)> = {
            let mut clients = self.clients.write().await;
            clients
                .drain()
                .filter_map(|(lang, state)| state.client().map(|c| (lang, c)))
                .collect()
        };

        for (lang, client) in clients_to_shutdown {
            if let Err(e) = client.shutdown().await {
                tracing::warn!("Error shutting down {:?} server: {}", lang, e);
            } else {
                tracing::info!("{:?} language server stopped", lang);
            }
        }
    }

    pub async fn cleanup_idle(&self, timeout: Duration) -> usize {
        let idle_languages: Vec<Language> = {
            let clients = self.clients.read().await;
            clients
                .iter()
                .filter(|(_, state)| state.idle_duration() > timeout)
                .filter_map(|(lang, state)| state.client().map(|_| *lang))
                .collect()
        };

        let mut stopped = 0;
        for lang in idle_languages {
            if self.shutdown_client(lang).await.is_ok() {
                tracing::info!("{:?} language server stopped (idle)", lang);
                stopped += 1;
            }
        }

        stopped
    }

    pub fn is_available(&self, language: Language) -> bool {
        self.configs
            .get(&language)
            .map(|c| c.is_installed())
            .unwrap_or(false)
    }

    /// Read-only peek at a pooled client — never starts one. Status
    /// queries must not have the side effect of booting a server.
    pub async fn peek_client(&self, language: Language) -> Option<Arc<LspClient>> {
        let clients = self.clients.read().await;
        clients.get(&language).and_then(|state| state.client())
    }

    pub async fn is_running(&self, language: Language) -> bool {
        let client = {
            let clients = self.clients.read().await;
            clients.get(&language).and_then(|s| s.client())
        };

        if let Some(client) = client {
            client.is_running().await
        } else {
            false
        }
    }

    pub async fn server_status(&self, language: Language) -> ServerStatusDetail {
        let config = match self.configs.get(&language) {
            Some(c) => c,
            None => return ServerStatusDetail::NotSupported,
        };

        if let Err(LspError::ServerNotInstalled { name, install_hint }) = config.resolve() {
            return ServerStatusDetail::NotInstalled { name, install_hint };
        }

        if self.is_running(language).await {
            return ServerStatusDetail::Running {
                name: config.display_name.to_string(),
                version: config.probe_version(),
            };
        }

        ServerStatusDetail::Stopped {
            name: config.display_name.to_string(),
            version: config.probe_version(),
        }
    }

    pub fn supported_languages(&self) -> Vec<Language> {
        self.configs.keys().copied().collect()
    }

    pub async fn running_languages(&self) -> Vec<Language> {
        let candidates: Vec<(Language, Arc<LspClient>)> = {
            let clients = self.clients.read().await;
            clients
                .iter()
                .filter_map(|(lang, state)| state.client().map(|c| (*lang, c)))
                .collect()
        };

        let mut running = Vec::new();
        for (lang, client) in candidates {
            if client.is_running().await {
                running.push(lang);
            }
        }
        running
    }

    pub async fn unhealthy_servers(&self) -> Vec<Language> {
        let candidates: Vec<(Language, Arc<LspClient>)> = {
            let clients = self.clients.read().await;
            clients
                .iter()
                .filter_map(|(lang, state)| state.client().map(|c| (*lang, c)))
                .collect()
        };

        let mut unhealthy = Vec::new();
        for (lang, client) in candidates {
            if !client.health_check().await {
                unhealthy.push(lang);
            }
        }
        unhealthy
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub fn runtime_config(&self) -> &crate::config::LspRuntimeConfig {
        &self.runtime_config
    }

    pub fn config(&self, language: Language) -> Option<&ServerConfig> {
        self.configs.get(&language)
    }

    pub async fn execute_with_retry<F, T, Fut>(
        &self,
        language: Language,
        op: F,
    ) -> Result<T, LspError>
    where
        F: Fn(Arc<LspClient>) -> Fut,
        Fut: Future<Output = Result<T, LspError>>,
    {
        use crate::infra::retry::{RetryConfig, with_retry};

        with_retry(&RetryConfig::for_language(language), || async {
            let client = self.get_client(language).await?;
            match op(Arc::clone(&client)).await {
                Ok(result) => Ok(result),
                Err(e) if e.needs_restart() && self.runtime_config.auto_restart => {
                    tracing::warn!("{:?} server error, restarting: {}", language, e);
                    Err(e)
                }
                Err(e) => Err(e),
            }
        })
        .await
    }
}

#[derive(Debug, Clone)]
pub enum ServerStatusDetail {
    Running {
        name: String,
        version: Option<String>,
    },
    Stopped {
        name: String,
        version: Option<String>,
    },
    NotInstalled {
        name: String,
        install_hint: String,
    },
    NotSupported,
}

impl std::fmt::Display for ServerStatusDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerStatusDetail::Running { name, version } => {
                if let Some(v) = version {
                    write!(f, "{} {} (running)", name, v)
                } else {
                    write!(f, "{} (running)", name)
                }
            }
            ServerStatusDetail::Stopped { name, version } => {
                if let Some(v) = version {
                    write!(f, "{} {} (stopped)", name, v)
                } else {
                    write!(f, "{} (stopped)", name)
                }
            }
            ServerStatusDetail::NotInstalled { name, install_hint } => {
                write!(f, "{} (not installed)\n  → Install: {}", name, install_hint)
            }
            ServerStatusDetail::NotSupported => write!(f, "Not supported"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with_cap(cap: usize) -> LspManager {
        let mut config = crate::config::LspRuntimeConfig::default();
        config.max_concurrent_servers = cap;
        LspManager::new(PathBuf::from("/test"), Arc::new(config))
    }

    #[test]
    fn eviction_returns_none_under_capacity() {
        let manager = manager_with_cap(4);
        let mut clients = HashMap::new();
        clients.insert(
            Language::Rust,
            ClientState::Initializing(Arc::new(Notify::new())),
        );
        assert_eq!(manager.pick_eviction_target(&clients), None);
    }

    #[test]
    fn eviction_never_selects_initializing_clients() {
        let manager = manager_with_cap(1);
        let mut clients = HashMap::new();
        clients.insert(
            Language::Rust,
            ClientState::Initializing(Arc::new(Notify::new())),
        );
        // Pool is at capacity but the only occupant is mid-startup:
        // nothing is evictable.
        assert_eq!(manager.pick_eviction_target(&clients), None);
    }

    #[test]
    fn test_server_status_display() {
        let status = ServerStatusDetail::Running {
            name: "rust-analyzer".to_string(),
            version: Some("2024-12-01".to_string()),
        };
        let display = status.to_string();
        assert!(display.contains("running"));
        assert!(display.contains("2024-12-01"));

        let status = ServerStatusDetail::NotInstalled {
            name: "pyright".to_string(),
            install_hint: "npm install -g pyright".to_string(),
        };
        let display = status.to_string();
        assert!(display.contains("not installed"));
        assert!(display.contains("npm"));
    }
}
