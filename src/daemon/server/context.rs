use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;

use crate::config::LspRuntimeConfig;
use crate::daemon::protocol::RpcError;
use crate::services::lsp::DefaultLspService;
use crate::services::store::{DefaultStoreService, StoreConfig};

pub(super) type ProjectsMap = Arc<RwLock<HashMap<PathBuf, Arc<ProjectContext>>>>;

pub(super) fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) struct ProjectContext {
    pub(super) lsp: Arc<DefaultLspService>,
    pub(super) store: DefaultStoreService,
    pub(super) last_used: AtomicU64,
    pub(super) request_count: AtomicU64,
}

impl ProjectContext {
    /// Construct a project's services without touching disk. The store opens
    /// lazily on its first use, so an LSP-only request never creates a
    /// `.symora` dir and a read-only project is served without error.
    pub(super) fn new(path: &std::path::Path, lsp_config: Arc<LspRuntimeConfig>) -> Self {
        Self {
            lsp: Arc::new(DefaultLspService::new(path, lsp_config)),
            store: DefaultStoreService::new(path, StoreConfig::default()),
            last_used: AtomicU64::new(epoch_millis()),
            request_count: AtomicU64::new(0),
        }
    }

    pub(super) fn touch(&self) {
        self.request_count.fetch_add(1, Ordering::Relaxed);
        self.last_used.store(epoch_millis(), Ordering::Relaxed);
    }

    pub(super) fn is_idle(&self, timeout: Duration) -> bool {
        let last = self.last_used.load(Ordering::Relaxed);
        epoch_millis().saturating_sub(last) > timeout.as_millis() as u64
    }
}

pub(super) async fn get_context(
    projects: &ProjectsMap,
    project: &str,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<Arc<ProjectContext>, RpcError> {
    let path = PathBuf::from(project);

    {
        let guard = projects.read().await;
        if let Some(ctx) = guard.get(&path) {
            return Ok(Arc::clone(ctx));
        }
    }

    let ctx = Arc::new(ProjectContext::new(&path, Arc::clone(lsp_config)));

    let mut guard = projects.write().await;
    if let Some(existing) = guard.get(&path) {
        return Ok(Arc::clone(existing));
    }
    guard.insert(path.clone(), Arc::clone(&ctx));
    Ok(ctx)
}
