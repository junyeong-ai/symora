//! Archive extraction (`.tar.gz`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::process::{have, run_streaming};

/// Extract a `.tar.gz` archive into `dest_dir` and return the path to the
/// expected `symora` binary within it. The directory must already exist.
pub fn extract_symora_archive(archive: &Path, dest_dir: &Path) -> Result<PathBuf> {
    if !have("tar") {
        return Err(anyhow!("tar is required to extract release archives"));
    }
    let archive_str = archive
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 archive path"))?;
    let dest_str = dest_dir
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 destination path"))?;
    run_streaming("tar", &["-xzf", archive_str, "-C", dest_str])
        .with_context(|| format!("extracting {}", archive.display()))?;

    let bin = dest_dir.join("symora");
    if !bin.is_file() {
        return Err(anyhow!("archive did not contain expected 'symora' binary"));
    }
    Ok(bin)
}
