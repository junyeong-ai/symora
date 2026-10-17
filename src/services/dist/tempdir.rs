use std::path::Path;

use anyhow::Result;

pub struct TempDir(tempfile::TempDir);

impl TempDir {
    pub fn new(prefix: &str) -> Result<Self> {
        Ok(Self(tempfile::Builder::new().prefix(prefix).tempdir()?))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}
