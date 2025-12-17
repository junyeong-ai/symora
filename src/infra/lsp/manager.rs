//! LSP Server Manager
//!
//! Manages multiple language server instances with race-safe access.

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
    Ready {
        client: Arc<LspClient>,
        last_used: Instant,
    },
}

impl ClientState {
    fn ready(client: Arc<LspClient>) -> Self {
        Self::Ready {
            client,
            last_used: Instant::now(),
        }
    }

    fn touch(&mut self) {
        if let Self::Ready { last_used, .. } = self {
            *last_used = Instant::now();
        }
    }

    fn idle_duration(&self) -> Duration {
        match self {
            Self::Ready { last_used, .. } => last_used.elapsed(),
            Self::Initializing(_) => Duration::ZERO,
        }
    }

    fn client(&self) -> Option<Arc<LspClient>> {
        match self {
            Self::Ready { client, .. } => Some(Arc::clone(client)),
            Self::Initializing(_) => None,
        }
    }
}

pub struct LspManager {
    root: PathBuf,
    clients: RwLock<HashMap<Language, ClientState>>,
    configs: HashMap<Language, ServerConfig>,
}

impl LspManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            clients: RwLock::new(HashMap::new()),
            configs: servers::defaults(),
        }
    }

    /// Get or start a client for a language (race-safe, deadlock-free)
    pub async fn get_client(&self, language: Language) -> Result<Arc<LspClient>, LspError> {
        loop {
            // Phase 1: Get client or notify under lock, release immediately
            let (client_opt, notify_opt) = {
                let clients = self.clients.read().await;
                match clients.get(&language) {
                    Some(ClientState::Ready { client, .. }) => (Some(Arc::clone(client)), None),
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

            // Phase 4: Start new client
            let notify = Arc::new(Notify::new());
            {
                let mut clients = self.clients.write().await;
                if clients.contains_key(&language) {
                    continue; // Race: another thread started, retry
                }
                clients.insert(language, ClientState::Initializing(Arc::clone(&notify)));
            }

            return self.start_client_internal(language, notify).await;
        }
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
                clients.insert(language, ClientState::ready(Arc::clone(client)));
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

        if !config.is_installed() {
            return Err(LspError::ServerNotInstalled {
                name: config.name.to_string(),
                install_hint: config.install.current().to_string(),
            });
        }

        let client = LspClient::new(language, self.root.clone());
        client.start(config.command, config.args).await?;

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
        let _ = self.shutdown_client(language).await;
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

    pub async fn server_status(&self, language: Language) -> ServerStatus {
        let config = match self.configs.get(&language) {
            Some(c) => c,
            None => return ServerStatus::NotSupported,
        };

        if !config.is_installed() {
            return ServerStatus::NotInstalled {
                name: config.name.to_string(),
                install_hint: config.install.current().to_string(),
            };
        }

        if self.is_running(language).await {
            return ServerStatus::Running {
                name: config.name.to_string(),
                version: config.version(),
            };
        }

        ServerStatus::Stopped {
            name: config.name.to_string(),
            version: config.version(),
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

    pub async fn idle_duration(&self, language: Language) -> Option<Duration> {
        let clients = self.clients.read().await;
        clients.get(&language).map(|state| state.idle_duration())
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
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
                Err(e) if e.needs_restart() && crate::config::auto_restart() => {
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
pub enum ServerStatus {
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

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerStatus::Running { name, version } => {
                if let Some(v) = version {
                    write!(f, "{} {} (running)", name, v)
                } else {
                    write!(f, "{} (running)", name)
                }
            }
            ServerStatus::Stopped { name, version } => {
                if let Some(v) = version {
                    write!(f, "{} {} (stopped)", name, v)
                } else {
                    write!(f, "{} (stopped)", name)
                }
            }
            ServerStatus::NotInstalled { name, install_hint } => {
                write!(f, "{} (not installed)\n  → Install: {}", name, install_hint)
            }
            ServerStatus::NotSupported => write!(f, "Not supported"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_status_display() {
        let status = ServerStatus::Running {
            name: "rust-analyzer".to_string(),
            version: Some("2024-12-01".to_string()),
        };
        let display = status.to_string();
        assert!(display.contains("running"));
        assert!(display.contains("2024-12-01"));

        let status = ServerStatus::NotInstalled {
            name: "pyright".to_string(),
            install_hint: "npm install -g pyright".to_string(),
        };
        let display = status.to_string();
        assert!(display.contains("not installed"));
        assert!(display.contains("npm"));
    }
}
