//! Writing an edit: the checks a target passes first, an atomic
//! replacement checked against the content it was planned on, the
//! project-wide edit lock, and the refresh of what answers about the files.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::app::App;
use crate::infra::file_lock::FileLock;

use super::stale_revision;

/// Maximum file size for editing (100MB)
const MAX_EDIT_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// Validate a file for editing. Existence and the size cap always apply;
/// `for_write` adds the writability probe.
pub(super) fn validate_file_for_edit(path: &Path, for_write: bool) -> Result<()> {
    use crate::cli::CliInputError;

    if !path.exists() {
        anyhow::bail!(CliInputError::FileNotFound(path.to_path_buf()));
    }

    let metadata = fs::metadata(path).context("Failed to read file metadata")?;

    if metadata.len() > MAX_EDIT_FILE_SIZE {
        anyhow::bail!(CliInputError::FileTooLarge {
            size_mb: metadata.len() / (1024 * 1024),
            limit_mb: MAX_EDIT_FILE_SIZE / (1024 * 1024),
        });
    }

    if for_write && fs::OpenOptions::new().write(true).open(path).is_err() {
        anyhow::bail!(CliInputError::FileNotWritable(path.to_path_buf()));
    }

    Ok(())
}

/// Write `content` to `target` atomically: stage into a dotfile in the
/// target's directory, fsync, carry the target's permissions over, rename
/// into place, then best-effort fsync the directory to make the rename
/// durable. A crash at any point can never leave a truncated source file —
/// the worst case is an orphaned staging file, which the drop guard removes
/// on every in-process failure path. Staging next to the target (not in a
/// temp dir) keeps the rename on one filesystem, where it is atomic.
pub(crate) fn atomic_write(target: &Path, content: &str) -> Result<()> {
    write_file(target, content, None)
}

pub(crate) fn lock_edits(root: &Path) -> Result<FileLock> {
    let state = crate::services::state_dir::create(root)?;
    match FileLock::exclusive(&state.join("edit.lock"))? {
        Some(lock) => Ok(lock),
        None => Err(stale_revision(
            "Another edit is running in this project; retry when it finishes".to_string(),
        )),
    }
}

pub(crate) fn write_file(target: &Path, content: &str, expected: Option<&str>) -> Result<()> {
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Target has no parent: {}", target.display()))?;
    let mut staging = tempfile::Builder::new()
        .prefix(".symora-edit-")
        .tempfile_in(parent)
        .with_context(|| format!("Failed to stage write: {}", target.display()))?;
    staging.write_all(content.as_bytes())?;
    if let Ok(meta) = fs::metadata(&target) {
        fs::set_permissions(staging.path(), meta.permissions())?;
    }
    staging.as_file().sync_all()?;
    if let Some(expected) = expected {
        let current = fs::read_to_string(&target)?;
        if current != expected {
            anyhow::bail!(stale_revision(format!(
                "{} changed while the edit was being prepared",
                target.display()
            )));
        }
    }
    staging
        .persist(&target)
        .with_context(|| format!("Failed to write file: {}", target.display()))?;
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Best-effort post-write catch-up for everything that answers about
/// files, through the same `StoreService`/`LspService` the rest of the
/// command layer uses — so it honors daemon/direct mode instead of
/// reaching for the daemon directly. The store re-extracts each file's
/// rows from the bytes just written (or drops them, if the file no longer
/// exists), so a search immediately after an edit sees the new content —
/// a store that was never built stays untouched. The LSP layer is told
/// the same files changed, so cached workspace answers expire and a live
/// server's overlay is synced and saved (fresh diagnostics on a warm
/// daemon). Failures are logged, never fail the edit that already landed.
pub(crate) async fn refresh_store_files(app: &App, files: &[PathBuf]) {
    if let Err(e) = app.store.refresh_files(files).await {
        tracing::warn!("Store refresh failed after edit: {e}");
    }
    app.lsp.note_files_edited(files).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_lock_keeps_the_state_directory_out_of_history() {
        let root = tempfile::tempdir().unwrap();
        let _lock = lock_edits(root.path()).unwrap();
        assert!(root.path().join(".symora/.gitignore").is_file());
    }

    #[test]
    fn a_write_preserves_foreign_staging_files_and_refuses_changed_content() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("a.py");
        fs::write(&target, "original").unwrap();
        let foreign = directory
            .path()
            .join(format!(".a.py.symora-edit.{}", std::process::id()));
        fs::write(&foreign, "owned by another operation").unwrap();
        atomic_write(&target, "current").unwrap();
        assert_eq!(
            fs::read_to_string(&foreign).unwrap(),
            "owned by another operation"
        );
        let error = write_file(&target, "replacement", Some("original")).unwrap_err();
        assert_eq!(
            crate::cli::OutputError::from(error).code,
            crate::cli::errors::ErrorCode::Conflict
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "current");
    }

    #[test]
    fn edits_of_one_project_require_exclusive_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let first = lock_edits(directory.path()).unwrap();
        assert!(lock_edits(directory.path()).is_err());
        drop(first);
        assert!(lock_edits(directory.path()).is_ok());
    }

    #[test]
    fn atomic_write_round_trips_content_including_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.rs");
        fs::write(&target, "old").unwrap();

        let content = "fn main() {}\r\nfn aux() {}\r\n";
        atomic_write(&target, content).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), content);
    }

    #[test]
    fn atomic_write_leaves_no_staging_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.rs");
        fs::write(&target, "old").unwrap();

        atomic_write(&target, "new").unwrap();
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "staging file leaked: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_target_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("script.sh");
        fs::write(&target, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(&target, "#!/bin/sh\necho hi\n").unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_edits_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.rs");
        let link = dir.path().join("link.rs");
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        atomic_write(&link, "new").unwrap();
        // The link must still be a symlink and the real file must hold the
        // new content — a rename onto the link itself would sever it.
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
    }
}
