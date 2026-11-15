use std::sync::atomic::Ordering;
use std::time::Duration;

use super::service::DefaultLspService;

impl DefaultLspService {
    pub(crate) fn has_pending_start(&self) -> bool {
        self.manager.has_pending_start()
    }

    pub async fn shutdown(&self) {
        self.health_shutdown.store(true, Ordering::Release);
        self.health_handle.abort();
        self.clear_caches().await;
        self.manager.shutdown_all().await;
    }

    pub async fn cleanup_idle(&self, timeout: Duration) -> usize {
        self.manager.cleanup_idle(timeout).await
    }

    pub async fn clear_caches(&self) {
        self.symbol_cache.clear().await;
        self.workspace_symbol_cache.clear().await;
    }

    pub async fn cleanup_expired_caches(&self) -> usize {
        self.symbol_cache.cleanup_expired().await
    }
}
