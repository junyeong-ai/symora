//! Tiny RAII temp directory used by `setup skill` and `self update`.
//! We deliberately do not pull in `tempfile` for runtime code — the dev
//! dependency exists for tests, and keeping this module dependency-free
//! mirrors the rest of `dist`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Result<Self> {
        let base = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = base.join(format!("{prefix}-{stamp}-{}", std::process::id()));
        std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(TempDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
