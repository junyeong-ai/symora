use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::LspRuntimeConfig;
use crate::error::LspError;
use crate::services::config::{load_global_config_sync, load_merged_config_sync};

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
    pub fn load() -> Result<Self, LspError> {
        let exe = std::env::current_exe().map_err(|e| {
            LspError::ServerStart(format!("Could not determine executable path: {e}"))
        })?;
        let id = installation_id(&exe)?;
        let base = crate::services::dist::paths::daemon_dir();
        let settings = Self::load_settings();

        Ok(Self {
            socket_path: base.join(format!("daemon-{id}.sock")),
            pid_path: base.join(format!("daemon-{id}.pid")),
            lock_path: base.join(format!("daemon-{id}.lock")),
            bind_lock_path: base.join(format!("daemon-{id}.bind.lock")),
            idle_timeout: Duration::from_secs(settings.idle_timeout_mins * 60),
            max_concurrent: settings.max_concurrent,
        })
    }

    fn load_settings() -> crate::models::config::DaemonConfig {
        load_global_config_sync()
            .map(|c| c.daemon)
            .unwrap_or_default()
    }

    pub fn load_lsp_config(root: &std::path::Path) -> Arc<LspRuntimeConfig> {
        let config = load_merged_config_sync(root)
            .ok()
            .map(|c| LspRuntimeConfig::from(&c))
            .unwrap_or_default();
        Arc::new(config)
    }
}

fn installation_id(exe: &Path) -> Result<String, LspError> {
    let canonical = exe.canonicalize().map_err(|e| {
        LspError::ServerStart(format!(
            "Could not determine canonical executable path for {}: {e}",
            exe.display()
        ))
    })?;
    Ok(path_id(&canonical))
}

// FNV-1a's fixed algorithm keeps installation keys stable across builds.
fn path_id(canonical: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_id_pins_fnv1a() {
        assert_eq!(
            path_id(Path::new("/opt/symora/bin/symora")),
            "6f208fd53683d3c9"
        );
        assert_eq!(
            path_id(Path::new("/opt/symora/3/bin/symora")),
            "0e9a5b26d7f1d56b"
        );
    }

    #[test]
    fn installation_id_depends_only_on_canonical_path() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, "build one").unwrap();
        std::fs::write(&second, "build one").unwrap();
        let id = installation_id(&first).unwrap();
        assert_eq!(id, installation_id(&first).unwrap());
        assert_ne!(id, installation_id(&second).unwrap());
        std::fs::write(&first, "rebuilt binary").unwrap();
        assert_eq!(id, installation_id(&first).unwrap());
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, "replacement build").unwrap();
        std::fs::rename(&replacement, &first).unwrap();
        assert_eq!(id, installation_id(&first).unwrap());
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        assert_eq!(id, installation_id(&link).unwrap());
    }

    #[test]
    fn installation_id_hashes_non_utf8_path_bytes() {
        let first = Path::new(std::ffi::OsStr::from_bytes(b"/opt/\xff/symora"));
        let second = Path::new(std::ffi::OsStr::from_bytes(b"/opt/\xfe/symora"));
        assert_ne!(path_id(first), path_id(second));
    }

    #[test]
    fn installation_id_requires_resolvable_executable() {
        let root = tempfile::tempdir().unwrap();
        let error = installation_id(&root.path().join("missing")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Could not determine canonical executable path")
        );
    }
}
