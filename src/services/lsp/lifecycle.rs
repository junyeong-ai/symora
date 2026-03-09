use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::service::DefaultLspService;
use crate::models::lsp::ServerStatus;
use crate::models::symbol::Language;

impl DefaultLspService {
    pub async fn shutdown(&self) {
        self.health_shutdown.store(true, Ordering::Release);
        self.health_handle.abort();
        self.clear_caches().await;
        self.manager.shutdown_all().await;
    }

    pub async fn cleanup_idle(&self, timeout: Duration) -> usize {
        self.manager.cleanup_idle(timeout).await
    }

    pub async fn invalidate_file_cache(&self, file: &Path) {
        self.symbol_cache.invalidate(file).await;
    }

    pub async fn clear_caches(&self) {
        self.symbol_cache.clear().await;
        self.workspace_symbol_cache.clear().await;
    }

    pub async fn cleanup_expired_caches(&self) -> usize {
        self.symbol_cache.cleanup_expired().await
    }
}

pub(super) async fn is_available(service: &DefaultLspService, language: Language) -> bool {
    service.manager.is_available(language)
}

pub(super) async fn server_status(service: &DefaultLspService, language: Language) -> ServerStatus {
    service.manager.server_status(language).await.into()
}
