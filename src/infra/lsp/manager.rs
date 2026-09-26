use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, watch};

use super::client::LspClient;
use super::servers::{self, ServerConfig};
use crate::error::LspError;
use crate::models::symbol::Language;

enum ClientState {
    /// A start is in flight. Its `StartSlot` holds the sending half; the
    /// channel closes when the slot drops, whatever ended the start.
    Initializing(watch::Receiver<()>),
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

enum Reservation {
    /// Another caller holds the entry; read it again.
    Taken,
    /// The pool is at capacity and every entry is mid-start.
    Full(watch::Receiver<()>),
    Granted {
        done: watch::Sender<()>,
        evict: Option<Language>,
    },
}

/// A start's claim on its pool entry. The start runs as its own task, so a
/// caller that stops waiting neither cancels it nor strands the entry: the
/// slot either fills the entry with the started client or, dropped without
/// filling it — failure, panic, runtime shutdown — clears it. Either way the
/// channel closes and every waiter re-reads the pool.
struct StartSlot {
    manager: Arc<LspManager>,
    language: Language,
    done: watch::Sender<()>,
}

impl StartSlot {
    async fn run(self, evict: Option<Language>) -> Result<Arc<LspClient>, LspError> {
        if let Some(victim) = evict
            && let Err(e) = self.manager.shutdown_client(victim).await
        {
            tracing::warn!(
                "Failed to evict {:?} before starting {:?}: {}",
                victim,
                self.language,
                e
            );
        }
        let client = self.manager.spawn_server(self.language).await?;
        self.fill(&client);
        Ok(client)
    }

    /// Hand the started client to the pool — unless a shutdown or restart
    /// took the entry meanwhile. Then the caller keeps the only handle, and
    /// the server stops when that caller is done with it.
    fn fill(&self, client: &Arc<LspClient>) {
        let mut clients = self.manager.pool();
        if self.holds(&clients) {
            clients.insert(self.language, ClientState::live(Arc::clone(client)));
        }
    }

    fn holds(&self, clients: &HashMap<Language, ClientState>) -> bool {
        matches!(
            clients.get(&self.language),
            Some(ClientState::Initializing(done)) if done.same_channel(&self.done.subscribe())
        )
    }
}

impl Drop for StartSlot {
    fn drop(&mut self) {
        let mut clients = self.manager.pool();
        if self.holds(&clients) {
            clients.remove(&self.language);
        }
    }
}

pub struct LspManager {
    root: PathBuf,
    /// Every section under this lock is synchronous, so a `StartSlot` can
    /// release its entry from `Drop`.
    clients: Mutex<HashMap<Language, ClientState>>,
    configs: HashMap<Language, ServerConfig>,
    runtime_config: Arc<crate::config::LspRuntimeConfig>,
    /// Languages the health monitor abandoned auto-restart on, with the
    /// reason. `server_status` reports these as `CriticalFailure` so a broken
    /// server is an honest terminal state, not a perpetual `Stopped`. Cleared
    /// when the health monitor next observes the language healthy.
    critical_failures: RwLock<HashMap<Language, String>>,
}

impl LspManager {
    pub fn new(root: PathBuf, runtime_config: Arc<crate::config::LspRuntimeConfig>) -> Self {
        Self {
            root,
            clients: Mutex::new(HashMap::new()),
            configs: servers::merged(&runtime_config.servers),
            runtime_config,
            critical_failures: RwLock::new(HashMap::new()),
        }
    }

    fn pool(&self) -> MutexGuard<'_, HashMap<Language, ClientState>> {
        self.clients.lock().expect("client pool lock poisoned")
    }

    /// Record that auto-restart was abandoned for a language. Called by the
    /// health monitor after repeated startup failures.
    pub async fn mark_critical_failure(&self, language: Language, reason: String) {
        self.critical_failures
            .write()
            .await
            .insert(language, reason);
    }

    /// Clear a critical-failure mark — the health monitor observed the language
    /// healthy again (the sole recovery point).
    pub async fn clear_critical_failure(&self, language: Language) {
        self.critical_failures.write().await.remove(&language);
    }

    pub async fn critical_failure_reason(&self, language: Language) -> Option<String> {
        self.critical_failures.read().await.get(&language).cloned()
    }

    /// The pooled client for a language, starting one when there is none.
    ///
    /// A client whose process has exited is replaced. Concurrent callers share
    /// one start; the caller that triggered it receives its error, and a caller
    /// that only waited on a failed start makes its own attempt.
    pub async fn get_client(
        self: &Arc<Self>,
        language: Language,
    ) -> Result<Arc<LspClient>, LspError> {
        loop {
            let pooled = self.pool().get(&language).map(|state| match state {
                ClientState::Live { client, .. } => Ok(Arc::clone(client)),
                ClientState::Initializing(done) => Err(done.clone()),
            });

            match pooled {
                Some(Ok(client)) => {
                    if client.is_running().await {
                        if let Some(state) = self.pool().get_mut(&language) {
                            state.touch();
                        }
                        return Ok(client);
                    }
                    self.retire(language, &client);
                    continue;
                }
                Some(Err(mut starting)) => {
                    let _ = starting.changed().await;
                    continue;
                }
                None => {}
            }

            let (done, evict) = match self.reserve(language) {
                Reservation::Taken => continue,
                Reservation::Full(mut settling) => {
                    let _ = settling.changed().await;
                    continue;
                }
                Reservation::Granted { done, evict } => (done, evict),
            };

            let slot = StartSlot {
                manager: Arc::clone(self),
                language,
                done,
            };
            return match tokio::spawn(slot.run(evict)).await {
                Ok(started) => started,
                Err(e) => std::panic::resume_unwind(e.into_panic()),
            };
        }
    }

    /// Claim the entry for a new start, choosing the least-recently-used
    /// client to evict when the pool is at capacity.
    fn reserve(&self, language: Language) -> Reservation {
        let mut clients = self.pool();
        if clients.contains_key(&language) {
            return Reservation::Taken;
        }
        let cap = self.runtime_config.max_concurrent_servers.max(1);
        let evict = self.pick_eviction_target(&clients);
        if clients.len() >= cap && evict.is_none() {
            // At capacity with nothing evictable: every occupant is
            // mid-startup. Wait for one to settle instead of exceeding the
            // cap with another Initializing entry.
            let settling = clients.values().find_map(|state| match state {
                ClientState::Initializing(done) => Some(done.clone()),
                ClientState::Live { .. } => None,
            });
            return Reservation::Full(
                settling.expect("a full pool with nothing to evict holds only starts"),
            );
        }
        let (done, starting) = watch::channel(());
        clients.insert(language, ClientState::Initializing(starting));
        Reservation::Granted { done, evict }
    }

    /// Drop a pooled client whose server exited, unless it was already
    /// replaced.
    fn retire(&self, language: Language, dead: &Arc<LspClient>) {
        let mut clients = self.pool();
        if matches!(
            clients.get(&language),
            Some(ClientState::Live { client, .. }) if Arc::ptr_eq(client, dead)
        ) {
            clients.remove(&language);
            tracing::warn!("{:?} language server exited; starting a new one", language);
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

    async fn spawn_server(&self, language: Language) -> Result<Arc<LspClient>, LspError> {
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
        let client = self
            .pool()
            .remove(&language)
            .and_then(|state| state.client());

        if let Some(client) = client {
            client.shutdown().await?;
            tracing::info!("{:?} language server stopped", language);
        }

        Ok(())
    }

    pub async fn restart_client(
        self: &Arc<Self>,
        language: Language,
    ) -> Result<Arc<LspClient>, LspError> {
        if let Err(e) = self.shutdown_client(language).await {
            tracing::warn!("Error shutting down {:?} before restart: {}", language, e);
        }
        tracing::info!("{:?} language server restarting", language);
        self.get_client(language).await
    }

    pub async fn shutdown_all(&self) {
        let clients_to_shutdown: Vec<(Language, Arc<LspClient>)> = self
            .pool()
            .drain()
            .filter_map(|(lang, state)| state.client().map(|c| (lang, c)))
            .collect();

        for (lang, client) in clients_to_shutdown {
            if let Err(e) = client.shutdown().await {
                tracing::warn!("Error shutting down {:?} server: {}", lang, e);
            } else {
                tracing::info!("{:?} language server stopped", lang);
            }
        }
    }

    pub async fn cleanup_idle(&self, timeout: Duration) -> usize {
        let idle_languages: Vec<Language> = self
            .pool()
            .iter()
            .filter(|(_, state)| state.idle_duration() > timeout)
            .filter_map(|(lang, state)| state.client().map(|_| *lang))
            .collect();

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
    pub fn peek_client(&self, language: Language) -> Option<Arc<LspClient>> {
        self.pool().get(&language).and_then(|state| state.client())
    }

    pub async fn is_running(&self, language: Language) -> bool {
        if let Some(client) = self.peek_client(language) {
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

        let install = match config.resolve() {
            Err(LspError::ServerNotInstalled { name, install_hint }) => Some((name, install_hint)),
            _ => None,
        };
        let critical = self.critical_failure_reason(language).await;
        let is_running = self.is_running(language).await;
        Self::resolve_status(config.display_name, install, critical, is_running, || {
            config.probe_version()
        })
    }

    /// Precedence for [`Self::server_status`], factored out so it is exercised
    /// without a live server. `NotInstalled` (cannot run) outranks a given-up
    /// `CriticalFailure` verdict, which outranks bare liveness — a pooled but
    /// UNHEALTHY server is alive yet broken, so the verdict must NOT be masked as
    /// `Running`. The health monitor is the sole clearer of that verdict (on
    /// observing the language running AND healthy), so a recovered server returns
    /// to `Running` within one monitor tick — never force-cleared here on
    /// liveness alone. Then liveness, then stopped.
    fn resolve_status(
        name: &str,
        install: Option<(String, String)>,
        critical: Option<String>,
        is_running: bool,
        version: impl FnOnce() -> Option<String>,
    ) -> ServerStatusDetail {
        if let Some((name, install_hint)) = install {
            return ServerStatusDetail::NotInstalled { name, install_hint };
        }
        if let Some(reason) = critical {
            return ServerStatusDetail::CriticalFailure {
                name: name.to_string(),
                reason,
            };
        }
        if is_running {
            return ServerStatusDetail::Running {
                name: name.to_string(),
                version: version(),
            };
        }
        ServerStatusDetail::Stopped {
            name: name.to_string(),
            version: version(),
        }
    }

    pub fn supported_languages(&self) -> Vec<Language> {
        self.configs.keys().copied().collect()
    }

    pub async fn running_languages(&self) -> Vec<Language> {
        let candidates = self.pooled_clients();

        let mut running = Vec::new();
        for (lang, client) in candidates {
            if client.is_running().await {
                running.push(lang);
            }
        }
        running
    }

    pub async fn unhealthy_servers(&self) -> Vec<Language> {
        let candidates = self.pooled_clients();

        let mut unhealthy = Vec::new();
        for (lang, client) in candidates {
            if !client.health_check().await {
                unhealthy.push(lang);
            }
        }
        unhealthy
    }

    fn pooled_clients(&self) -> Vec<(Language, Arc<LspClient>)> {
        self.pool()
            .iter()
            .filter_map(|(lang, state)| state.client().map(|c| (*lang, c)))
            .collect()
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
        self: &Arc<Self>,
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
    /// The health monitor abandoned auto-restart after repeated startup
    /// failures — surfaced honestly instead of a perpetual `Stopped`/`timed_out`.
    CriticalFailure {
        name: String,
        reason: String,
    },
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
            ServerStatusDetail::CriticalFailure { name, reason } => {
                write!(f, "{name} (critical failure: {reason})")
            }
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
    fn server_status_precedence_critical_failure_outranks_liveness() {
        use ServerStatusDetail::*;
        // The give-up verdict must win over a bare liveness check: a pooled but
        // UNHEALTHY server is alive yet broken, so it is CriticalFailure, never
        // masked as Running. (This precedence regressed once — pin it.)
        assert!(matches!(
            LspManager::resolve_status(
                "rust-analyzer",
                None,
                Some("gave up".to_string()),
                true,
                || None
            ),
            CriticalFailure { .. }
        ));
        // NotInstalled cannot run, so it outranks everything else.
        assert!(matches!(
            LspManager::resolve_status(
                "rust-analyzer",
                Some((
                    "rust-analyzer".to_string(),
                    "rustup component add".to_string()
                )),
                Some("gave up".to_string()),
                true,
                || None,
            ),
            NotInstalled { .. }
        ));
        // With no verdict: liveness decides Running vs Stopped.
        assert!(matches!(
            LspManager::resolve_status("rust-analyzer", None, None, true, || None),
            Running { .. }
        ));
        assert!(matches!(
            LspManager::resolve_status("rust-analyzer", None, None, false, || None),
            Stopped { .. }
        ));
    }

    #[test]
    fn eviction_returns_none_under_capacity() {
        let manager = manager_with_cap(4);
        let mut clients = HashMap::new();
        clients.insert(
            Language::Rust,
            ClientState::Initializing(watch::channel(()).1),
        );
        assert_eq!(manager.pick_eviction_target(&clients), None);
    }

    #[test]
    fn eviction_never_selects_initializing_clients() {
        let manager = manager_with_cap(1);
        let mut clients = HashMap::new();
        clients.insert(
            Language::Rust,
            ClientState::Initializing(watch::channel(()).1),
        );
        // Pool is at capacity but the only occupant is mid-startup:
        // nothing is evictable.
        assert_eq!(manager.pick_eviction_target(&clients), None);
    }

    #[tokio::test]
    async fn critical_failure_registry_marks_and_clears() {
        let manager = manager_with_cap(4);
        assert!(
            manager
                .critical_failure_reason(Language::Rust)
                .await
                .is_none()
        );

        manager
            .mark_critical_failure(Language::Rust, "init crashed 3×".to_string())
            .await;
        assert_eq!(
            manager
                .critical_failure_reason(Language::Rust)
                .await
                .as_deref(),
            Some("init crashed 3×"),
        );

        // A clean restart / recovery clears the verdict — it is not permanent.
        manager.clear_critical_failure(Language::Rust).await;
        assert!(
            manager
                .critical_failure_reason(Language::Rust)
                .await
                .is_none()
        );
    }

    /// A stand-in language server that records its pid and, like a real
    /// server, ignores stdin EOF — so a process that goes away was stopped
    /// by the client, never by its own shutdown logic.
    #[cfg(unix)]
    mod server_lifetime {
        use super::*;
        use std::time::Duration;

        const FAKE_SERVER: &str = r#"#!/bin/sh
echo $$ >> "$1"
case "$2" in
  reject) body='{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"no workspace"}}' ;;
  serve) body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}' ;;
  hang) exec sleep 600 ;;
esac
IFS= read -r _
sleep "$3"
printf 'Content-Length: %s\r\n\r\n%s' "${#body}" "$body"
exec sleep 600
"#;

        struct FakeServer {
            dir: tempfile::TempDir,
        }

        impl FakeServer {
            fn new() -> Self {
                use std::os::unix::fs::PermissionsExt;
                let dir = tempfile::tempdir().unwrap();
                let script = dir.path().join("fake-ls");
                std::fs::write(&script, FAKE_SERVER).unwrap();
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
                Self { dir }
            }

            /// A pool whose Go server is the fake, answering `initialize`
            /// per `behavior` after `delay_secs`. Go's profile makes the
            /// handshake budget 2s at `timeout_secs = 1`.
            fn manager(&self, behavior: &str, delay_secs: u32) -> Arc<LspManager> {
                let mut config = crate::models::config::SymoraConfig::default();
                config.lsp.timeout_secs = 1;
                config.lsp.servers.insert(
                    "go".to_string(),
                    crate::models::config::ServerOverride {
                        command: Some(self.dir.path().join("fake-ls").display().to_string()),
                        args: Some(vec![
                            self.dir.path().join("pids").display().to_string(),
                            behavior.to_string(),
                            delay_secs.to_string(),
                        ]),
                        tier: None,
                    },
                );
                Arc::new(LspManager::new(
                    self.dir.path().to_path_buf(),
                    Arc::new(crate::config::LspRuntimeConfig::from(&config)),
                ))
            }

            fn pids(&self) -> Vec<u32> {
                std::fs::read_to_string(self.dir.path().join("pids"))
                    .unwrap_or_default()
                    .lines()
                    .map(|line| line.parse().unwrap())
                    .collect()
            }

            /// The process state `ps` reports, empty once the pid is reaped.
            fn stat(pid: u32) -> String {
                std::process::Command::new("ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .unwrap_or_default()
            }

            fn alive(pid: u32) -> bool {
                !Self::stat(pid).is_empty()
            }

            async fn kill(pid: u32) {
                std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .status()
                    .unwrap();
                while !Self::stat(pid).is_empty() && !Self::stat(pid).starts_with('Z') {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }

            /// Every server this fake ever started has exited AND been
            /// reaped — a zombie still counts as a leak.
            async fn assert_all_gone(&self) {
                let pids = self.pids();
                assert!(!pids.is_empty(), "the fake server never started");
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                while pids.iter().any(|pid| Self::alive(*pid)) {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "server processes outlived their handles: {:?}",
                        pids.iter()
                            .filter(|pid| Self::alive(**pid))
                            .collect::<Vec<_>>()
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }

        async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
            tokio::time::timeout(Duration::from_secs(15), future)
                .await
                .expect("the pool stopped making progress")
        }

        #[tokio::test]
        async fn a_server_that_rejects_initialize_is_terminated() {
            let fake = FakeServer::new();
            let manager = fake.manager("reject", 0);
            for _ in 0..3 {
                assert!(bounded(manager.get_client(Language::Go)).await.is_err());
            }
            assert_eq!(fake.pids().len(), 3);
            fake.assert_all_gone().await;
        }

        #[tokio::test]
        async fn an_abandoned_start_neither_leaks_nor_blocks_the_next_caller() {
            let fake = FakeServer::new();
            let manager = fake.manager("hang", 0);
            let abandoned =
                tokio::time::timeout(Duration::from_millis(200), manager.get_client(Language::Go))
                    .await;
            assert!(abandoned.is_err());

            assert!(bounded(manager.get_client(Language::Go)).await.is_err());
            fake.assert_all_gone().await;
        }

        #[tokio::test]
        async fn a_start_outlives_the_caller_that_triggered_it() {
            let fake = FakeServer::new();
            let manager = fake.manager("serve", 1);
            let abandoned =
                tokio::time::timeout(Duration::from_millis(200), manager.get_client(Language::Go))
                    .await;
            assert!(abandoned.is_err());

            let client = bounded(manager.get_client(Language::Go)).await.unwrap();
            assert!(client.is_running().await);
            assert_eq!(
                fake.pids().len(),
                1,
                "the second caller joined the first start"
            );

            drop((client, manager));
            fake.assert_all_gone().await;
        }

        #[tokio::test]
        async fn a_dead_server_is_replaced() {
            let fake = FakeServer::new();
            let manager = fake.manager("serve", 0);
            bounded(manager.get_client(Language::Go)).await.unwrap();
            FakeServer::kill(fake.pids()[0]).await;

            let replacement = bounded(manager.get_client(Language::Go)).await.unwrap();
            assert!(replacement.is_running().await);
            assert_eq!(fake.pids().len(), 2);

            drop((replacement, manager));
            fake.assert_all_gone().await;
        }

        #[tokio::test]
        async fn dropping_the_pool_terminates_its_servers() {
            let fake = FakeServer::new();
            let manager = fake.manager("serve", 0);
            bounded(manager.get_client(Language::Go)).await.unwrap();

            drop(manager);
            fake.assert_all_gone().await;
        }
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
