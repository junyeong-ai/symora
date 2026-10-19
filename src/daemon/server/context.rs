use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;

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
}

impl ProjectContext {
    /// Construct a project's services from that project's own configuration.
    ///
    /// A daemon serves many projects and their settings are theirs — a server
    /// override, a timeout, a size ceiling — so the config is read from the
    /// path being served, never from wherever the daemon happened to start.
    /// Reading it here is what makes a daemon answer agree with a direct one.
    ///
    /// The store opens lazily on its first use, so an LSP-only request never
    /// creates a `.symora` dir and a read-only project is served without error.
    pub(super) fn new(path: &std::path::Path) -> Self {
        let lsp_config = DaemonRuntimeConfig::load_lsp_config(path);
        let store = DefaultStoreService::new(path, crate::app::store_config(&lsp_config));
        Self {
            config: Arc::clone(&lsp_config),
            lsp: Arc::new(DefaultLspService::new(path, lsp_config, FileWatch::On)),
            store,
            last_used: AtomicU64::new(epoch_millis()),
            request_count: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
        }
    }

    pub(super) fn is_idle(&self, timeout: Duration) -> bool {
        if self.is_busy() {
            return false;
        }
        let last = self.last_used.load(Ordering::Relaxed);
        epoch_millis().saturating_sub(last) > timeout.as_millis() as u64
    }

    fn is_busy(&self) -> bool {
        self.in_flight.load(Ordering::Acquire) != 0 || self.lsp.has_pending_start()
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

pub(super) async fn get_context(
    projects: &ProjectsMap,
    project: &str,
) -> Result<ProjectLease, RpcError> {
    let path = PathBuf::from(project);

    {
        let guard = projects.read().await;
        if let Some(ctx) = guard.get(&path) {
            return Ok(ProjectLease::acquire(ctx));
        }
    }

    let mut guard = projects.write().await;
    if let Some(existing) = guard.get(&path) {
        return Ok(ProjectLease::acquire(existing));
    }
    let ctx = Arc::new(ProjectContext::new(&path));
    guard.insert(path, Arc::clone(&ctx));
    Ok(ProjectLease::acquire(&ctx))
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
    context.lsp.shutdown().await;
    ProjectRestartStatus::Restarted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restart_refuses_a_server_start_after_its_caller_is_cancelled() {
        use crate::services::lsp::LspService;

        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("ready");
        let release = root.path().join("release");
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lsp/configuration_server.py");
        let mut config = crate::models::config::SymoraConfig::default();
        config.lsp.timeout_secs = 5;
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
        tokio::time::timeout(Duration::from_secs(5), async {
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
        let restarted = tokio::time::timeout(Duration::from_secs(5), async {
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

    #[tokio::test]
    async fn live_lease_defers_idle_time_until_release() {
        let root = tempfile::tempdir().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let lease = get_context(&projects, root.path().to_str().unwrap())
            .await
            .unwrap();
        let ctx = Arc::clone(&lease);
        ctx.last_used
            .store(epoch_millis() - 1000, Ordering::Relaxed);
        assert!(!ctx.is_idle(Duration::ZERO));
        let released_after = epoch_millis();
        drop(lease);
        assert_eq!(ctx.in_flight.load(Ordering::Acquire), 0);
        assert!(ctx.last_used.load(Ordering::Relaxed) >= released_after);
        assert!(!ctx.is_idle(Duration::from_secs(1)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(ctx.is_idle(Duration::from_millis(10)));
    }

    #[tokio::test]
    async fn resolution_excludes_concurrent_idle_eviction() {
        let root = tempfile::tempdir().unwrap();
        let mut config = DaemonRuntimeConfig::load().unwrap();
        config.idle_timeout = Duration::ZERO;
        let server = super::super::DaemonServer::new(config);
        let ctx = Arc::new(ProjectContext::new(root.path()));
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
        let ctx = ProjectContext::new(root.path());
        assert_eq!(ctx.request_count.load(Ordering::Relaxed), 0);
        assert!(!ctx.is_idle(Duration::from_secs(1)));
        ctx.last_used
            .store(epoch_millis() - 1000, Ordering::Relaxed);
        assert!(ctx.is_idle(Duration::from_millis(10)));
    }
}
