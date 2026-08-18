use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::LspRuntimeConfig;
use crate::models::config::SymoraConfig;
use crate::services::config::load_merged_config_sync;

#[derive(Debug, Clone)]
pub struct DaemonRuntimeConfig {
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub lock_path: PathBuf,
    /// Guards the moment a server claims the socket path, so a probe and
    /// the bind that follows it cannot be split by another server.
    pub bind_lock_path: PathBuf,
    pub idle_timeout: Duration,
    pub max_concurrent: usize,
}

impl DaemonRuntimeConfig {
    pub fn load(root: &std::path::Path) -> Self {
        let base = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".symora");

        let settings = Self::load_settings(root);

        Self {
            socket_path: base.join("daemon.sock"),
            pid_path: base.join("daemon.pid"),
            lock_path: base.join("daemon.lock"),
            bind_lock_path: base.join("daemon.bind.lock"),
            idle_timeout: Duration::from_secs(settings.idle_timeout_mins * 60),
            max_concurrent: settings.max_concurrent,
        }
    }

    fn load_settings(root: &std::path::Path) -> crate::models::config::DaemonConfig {
        Self::load_config(root)
            .map(|c| c.daemon)
            .unwrap_or_default()
    }

    fn load_config(root: &std::path::Path) -> Option<SymoraConfig> {
        load_merged_config_sync(root, false).ok()
    }

    pub fn load_lsp_config(root: &std::path::Path) -> Arc<LspRuntimeConfig> {
        let config = Self::load_config(root)
            .map(|c| LspRuntimeConfig::from(&c))
            .unwrap_or_default();
        Arc::new(config)
    }
}
