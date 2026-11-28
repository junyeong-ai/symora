use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use tokio::sync::{Mutex, RwLock};

use super::config::DaemonRuntimeConfig;
use crate::daemon::protocol::RpcError;
use crate::daemon::wire::ProjectRestartStatus;
use crate::infra::lsp::watch::FileWatch;
use crate::services::lsp::DefaultLspService;
use crate::services::store::DefaultStoreService;

pub(super) type ProjectsMap = Arc<RwLock<HashMap<PathBuf, Arc<ProjectContext>>>>;

pub(super) fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) struct ProjectContext {
    pub(super) config: Arc<crate::config::LspRuntimeConfig>,
    pub(super) lsp: Arc<DefaultLspService>,
    pub(super) store: DefaultStoreService,
    last_used: AtomicU64,
    in_flight: AtomicU64,
    pub(super) request_count: AtomicU64,
    /// The context this one replaced, until it is stopped
    /// ([`Self::retire_predecessor`]). Held while it is being stopped, so
    /// whoever waits on it waits for its servers to exit.
    predecessor: Mutex<Option<Arc<ProjectContext>>>,
}

impl ProjectContext {
    /// Construct a project's services from that project's own configuration.
    ///
    /// A daemon serves many projects and their settings are theirs — a server
    /// override, a timeout, a size ceiling — so the config a context is built
    /// from is read from the path being served, never from wherever the
    /// daemon happened to start.
    ///
    /// The store opens lazily on its first use, and a read never creates one,
    /// so no query writes a `.symora` dir and a read-only project is served
    /// without error.
    fn new(
        path: &Path,
        config: Arc<crate::config::LspRuntimeConfig>,
        predecessor: Option<Arc<ProjectContext>>,
    ) -> Self {
        let store = DefaultStoreService::new(path, crate::app::store_config(&config));
        Self {
            lsp: Arc::new(DefaultLspService::new(
                path,
                Arc::clone(&config),
                FileWatch::On,
            )),
            config,
            store,
            last_used: AtomicU64::new(epoch_millis()),
            request_count: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            predecessor: Mutex::new(predecessor),
        }
    }

    pub(super) fn is_idle(&self, timeout: Duration) -> bool {
        if self.is_busy() {
            return false;
        }
        let last = self.last_used.load(Ordering::Relaxed);
        epoch_millis().saturating_sub(last) > timeout.as_millis() as u64
    }

    /// A request or a server start holds this context, or the one it
    /// replaced, or that one is being stopped.
    fn is_busy(&self) -> bool {
        self.in_flight.load(Ordering::Acquire) != 0
            || self.lsp.has_pending_start()
            || self.predecessor.try_lock().map_or(true, |predecessor| {
                predecessor
                    .as_ref()
                    .is_some_and(|predecessor| predecessor.is_busy())
            })
    }

    /// Stop the context this one replaced once nothing holds it, and wait
    /// for a stop already under way, so a request that goes on to start this
    /// context's servers never runs them beside the old ones. Nothing can
    /// take hold of the old context again: it is in no map, and a lease is
    /// shared only from another.
    pub(super) async fn retire_predecessor(&self) {
        let mut predecessor = self.predecessor.lock().await;
        if let Some(idle) = predecessor.take_if(|predecessor| !predecessor.is_busy()) {
            idle.stop().await;
        }
    }

    /// Checkpoint the store and stop the language servers, this context's
    /// and those of the one it replaced.
    pub(super) async fn stop(&self) {
        if let Err(e) = self.store.checkpoint().await {
            tracing::debug!("Failed to checkpoint a project's store: {}", e);
        }
        self.lsp.shutdown().await;
        let predecessor = self.predecessor.lock().await.take();
        if let Some(predecessor) = predecessor {
            Box::pin(predecessor.stop()).await;
        }
    }
}

#[cfg(test)]
impl ProjectContext {
    /// The context a first request for `path` builds.
    pub(super) fn load(path: &Path) -> Self {
        Self::new(path, DaemonRuntimeConfig::load_lsp_config(path), None)
    }
}

pub(super) struct ProjectLease {
    context: Arc<ProjectContext>,
}

impl ProjectLease {
    /// Acquisition must hold the projects-map lock to exclude idle eviction.
    fn acquire(context: &Arc<ProjectContext>) -> Self {
        context.in_flight.fetch_add(1, Ordering::Relaxed);
        context.request_count.fetch_add(1, Ordering::Relaxed);
        Self {
            context: Arc::clone(context),
        }
    }

    /// Another lease on the same project, for work that outlives the request
    /// holding this one. This lease already keeps the project from idle
    /// eviction, so the new one needs no projects-map lock to be taken safely.
    pub(super) fn share(&self) -> Self {
        self.context.in_flight.fetch_add(1, Ordering::Relaxed);
        Self {
            context: Arc::clone(&self.context),
        }
    }
}

impl std::ops::Deref for ProjectLease {
    type Target = Arc<ProjectContext>;

    fn deref(&self) -> &Self::Target {
        &self.context
    }
}

impl Drop for ProjectLease {
    fn drop(&mut self) {
        // Publish the release time before an idle observer can see zero leases.
        // Concurrent releases must not replace a later stamp with an earlier one.
        self.context
            .last_used
            .fetch_max(epoch_millis(), Ordering::Relaxed);
        self.context.in_flight.fetch_sub(1, Ordering::Release);
    }
}

/// A lease on the context serving `project` under the configuration it holds
/// now, as a direct run reads it. A context built from other settings is
/// replaced by one built from these, which stops the old one before serving
/// where nothing holds the old one, and otherwise once nothing does: a
/// project's servers are replaced rather than run twice over, except while
/// requests still need the old ones.
pub(super) async fn get_context(
    projects: &ProjectsMap,
    project: &str,
) -> Result<ProjectLease, RpcError> {
    let path = PathBuf::from(project);
    let config = DaemonRuntimeConfig::load_lsp_config(&path);
    let lease = lease_under(projects, path, config).await;
    lease.retire_predecessor().await;
    Ok(lease)
}

async fn lease_under(
    projects: &ProjectsMap,
    path: PathBuf,
    config: Arc<crate::config::LspRuntimeConfig>,
) -> ProjectLease {
    {
        let guard = projects.read().await;
        if let Some(ctx) = guard.get(&path).filter(|ctx| ctx.config == config) {
            return ProjectLease::acquire(ctx);
        }
    }
    let mut guard = projects.write().await;
    if let Some(ctx) = guard.get(&path).filter(|ctx| ctx.config == config) {
        return ProjectLease::acquire(ctx);
    }
    let previous = guard.remove(&path);
    let ctx = Arc::new(ProjectContext::new(&path, config, previous));
    guard.insert(path, Arc::clone(&ctx));
    ProjectLease::acquire(&ctx)
}

pub(super) async fn restart_project(projects: &ProjectsMap, project: &str) -> ProjectRestartStatus {
    let context = {
        let mut guard = projects.write().await;
        let path = std::path::Path::new(project);
        let Some(context) = guard.get(path) else {
            return ProjectRestartStatus::NotLoaded;
        };
        if context.is_busy() {
            return ProjectRestartStatus::Busy;
        }
        guard.remove(path).unwrap()
    };
    context.stop().await;
    ProjectRestartStatus::Restarted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::lsp::manager::tests::IDLE_LIMIT;

    #[tokio::test]
    async fn restart_refuses_a_server_start_after_its_caller_is_cancelled() {
        use crate::services::lsp::LspService;

        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("ready");
        let release = root.path().join("release");
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lsp/configuration_server.py");
        // Generous enough for python3 to start on a loaded machine; the
        // request is cancelled once the server is ready, so a passing run
        // never waits it out.
        let budget = 10;
        let mut config = crate::models::config::SymoraConfig::default();
        config.lsp.timeout_secs = budget;
        config.lsp.servers.insert(
            "python".into(),
            crate::models::config::ServerOverride {
                command: Some("python3".into()),
                args: Some(vec![
                    fixture.display().to_string(),
                    ready.display().to_string(),
                    release.display().to_string(),
                ]),
                ..Default::default()
            },
        );
        std::fs::create_dir(root.path().join(".symora")).unwrap();
        std::fs::write(
            root.path().join(".symora/config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        std::fs::write(root.path().join("main.py"), "value = 1\n").unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let request = tokio::spawn({
            let projects = Arc::clone(&projects);
            let path = root.path().to_path_buf();
            async move {
                let lease = get_context(&projects, path.to_str().unwrap())
                    .await
                    .unwrap();
                lease.lsp.hover(&path.join("main.py"), 1, 1).await
            }
        });
        tokio::time::timeout(Duration::from_secs(2 * budget), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        {
            let guard = projects.read().await;
            let context = guard.get(root.path()).unwrap();
            assert_eq!(context.in_flight.load(Ordering::Acquire), 0);
            context
                .last_used
                .store(epoch_millis() - 1000, Ordering::Relaxed);
            assert!(!context.is_idle(Duration::ZERO));
        }
        let path = root.path().to_str().unwrap();
        let outcome = restart_project(&projects, path).await;
        std::fs::write(&release, "").unwrap();
        assert_eq!(outcome, ProjectRestartStatus::Busy);
        let restarted = tokio::time::timeout(Duration::from_secs(2 * budget), async {
            loop {
                let outcome = restart_project(&projects, path).await;
                if outcome != ProjectRestartStatus::Busy {
                    break outcome;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(restarted, ProjectRestartStatus::Restarted);
        let pid = std::fs::read_to_string(&ready)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn restart_preserves_busy_requests_other_projects_and_reloads_config() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let path = first.path().to_str().unwrap();
        assert_eq!(
            restart_project(&projects, path).await,
            ProjectRestartStatus::NotLoaded
        );
        assert!(projects.read().await.is_empty());

        let lease = get_context(&projects, path).await.unwrap();
        let previous = Arc::clone(&lease);
        let other = get_context(&projects, second.path().to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            restart_project(&projects, path).await,
            ProjectRestartStatus::Busy
        );
        assert!(Arc::ptr_eq(
            projects.read().await.get(first.path()).unwrap(),
            &previous
        ));
        drop(lease);

        std::fs::create_dir(first.path().join(".symora")).unwrap();
        std::fs::write(
            first.path().join(".symora/config.toml"),
            "[lsp]\ntimeout_secs = 17\n",
        )
        .unwrap();
        assert_eq!(
            restart_project(&projects, path).await,
            ProjectRestartStatus::Restarted
        );
        {
            let guard = projects.read().await;
            assert!(!guard.contains_key(first.path()));
            assert!(Arc::ptr_eq(guard.get(second.path()).unwrap(), &other));
        }
        let fresh = get_context(&projects, path).await.unwrap();
        assert!(!Arc::ptr_eq(&fresh, &previous));
        assert_eq!(fresh.config.request_timeout(), Duration::from_secs(17));
    }

    /// A request is served under the settings its project holds as it
    /// arrives: an edit that reads the same keeps the context, and one that
    /// does not replaces it, stopping the old one at once where nothing holds
    /// it and otherwise once what holds it lets go, the project counting as
    /// busy until then.
    #[tokio::test]
    async fn a_request_is_served_under_the_settings_its_project_holds_now() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_str().unwrap();
        std::fs::create_dir(root.path().join(".symora")).unwrap();
        let configure = |text: &str| {
            std::fs::write(root.path().join(".symora/config.toml"), text).unwrap();
        };
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));

        configure("[lsp]\ntimeout_secs = 17\n");
        let held = get_context(&projects, path).await.unwrap();
        configure("# the same settings\n[lsp]\ntimeout_secs = 17\n");
        assert!(Arc::ptr_eq(
            &*get_context(&projects, path).await.unwrap(),
            &held
        ));

        configure("[lsp]\ntimeout_secs = 19\n");
        let successor = Arc::clone(&get_context(&projects, path).await.unwrap());
        assert!(!Arc::ptr_eq(&successor, &held));
        assert_eq!(successor.config.request_timeout(), Duration::from_secs(19));
        assert!(!successor.is_idle(Duration::ZERO));
        assert_eq!(
            restart_project(&projects, path).await,
            ProjectRestartStatus::Busy
        );
        successor.retire_predecessor().await;
        assert!(successor.predecessor.lock().await.is_some());
        drop(held);
        successor.retire_predecessor().await;
        assert!(successor.predecessor.lock().await.is_none());

        configure("[lsp]\ntimeout_secs = 23\n");
        let next = get_context(&projects, path).await.unwrap();
        assert_eq!(next.config.request_timeout(), Duration::from_secs(23));
        assert!(next.predecessor.lock().await.is_none());
    }

    #[tokio::test]
    async fn live_lease_defers_idle_time_until_release() {
        let root = tempfile::tempdir().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let lease = get_context(&projects, root.path().to_str().unwrap())
            .await
            .unwrap();
        let ctx = Arc::clone(&lease);
        ctx.last_used.store(
            epoch_millis() - (2 * IDLE_LIMIT).as_millis() as u64,
            Ordering::Relaxed,
        );
        assert!(!ctx.is_idle(Duration::ZERO));
        let released_after = epoch_millis();
        drop(lease);
        assert_eq!(ctx.in_flight.load(Ordering::Acquire), 0);
        assert!(ctx.last_used.load(Ordering::Relaxed) >= released_after);
        assert!(!ctx.is_idle(IDLE_LIMIT));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(ctx.is_idle(Duration::from_millis(10)));
    }

    #[tokio::test]
    async fn resolution_excludes_concurrent_idle_eviction() {
        let root = tempfile::tempdir().unwrap();
        let mut config = DaemonRuntimeConfig::load().unwrap();
        config.idle_timeout = Duration::ZERO;
        let server = super::super::DaemonServer::new(config);
        let ctx = Arc::new(ProjectContext::load(root.path()));
        ctx.last_used
            .store(epoch_millis() - 1000, Ordering::Relaxed);
        server
            .projects
            .write()
            .await
            .insert(root.path().to_path_buf(), Arc::clone(&ctx));
        let (lease, ()) = tokio::join!(
            biased;
            get_context(&server.projects, root.path().to_str().unwrap()),
            server.cleanup_idle_servers(),
        );
        let lease = lease.unwrap();
        let projects = server.projects.read().await;
        assert!(Arc::ptr_eq(projects.get(root.path()).unwrap(), &lease));
        assert!(Arc::ptr_eq(&ctx, &lease));
        assert!(!lease.is_idle(Duration::ZERO));
    }

    #[tokio::test]
    async fn overlapping_leases_stay_busy_until_both_release() {
        let root = tempfile::tempdir().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let first = get_context(&projects, root.path().to_str().unwrap())
            .await
            .unwrap();
        let second = get_context(&projects, root.path().to_str().unwrap())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let ctx = Arc::clone(&first);
        drop(first);
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(!ctx.is_idle(Duration::ZERO));
        drop(second);
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(ctx.is_idle(Duration::ZERO));
    }

    #[tokio::test]
    async fn unused_project_counts_idle_time_from_creation() {
        let root = tempfile::tempdir().unwrap();
        let ctx = ProjectContext::load(root.path());
        assert_eq!(ctx.request_count.load(Ordering::Relaxed), 0);
        assert!(!ctx.is_idle(IDLE_LIMIT));
        ctx.last_used
            .store(epoch_millis() - 1000, Ordering::Relaxed);
        assert!(ctx.is_idle(Duration::from_millis(10)));
    }
}
