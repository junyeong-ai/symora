//! Advisory locks over a file, for the state two symora processes share.
//!
//! A daemon and a direct run touch the same index and the same socket, so
//! some operations need a turn rather than a merge. An OS file lock (`flock`
//! on Unix) gives that with the one property a lease cannot: the OS releases
//! it however the holder ends, so a killed process leaves nothing to time out
//! or reclaim by guesswork. Every lock is scoped to the value's lifetime.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

pub struct FileLock {
    file: File,
}

impl FileLock {
    /// Take the lock for an operation nothing else may overlap. `Ok(None)`
    /// is contention — someone holds it — while an error means the lock
    /// could not be attempted at all, which is a different answer and must
    /// not be reported as a busy peer.
    pub fn exclusive(path: &Path) -> io::Result<Option<Self>> {
        Self::acquire(path, Mode::Exclusive)
    }

    /// Take the lock for an operation that tolerates its own kind but not
    /// an exclusive holder.
    pub fn shared(path: &Path) -> io::Result<Option<Self>> {
        Self::acquire(path, Mode::Shared)
    }

    fn acquire(path: &Path, mode: Mode) -> io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match lock(&file, mode) {
            Ok(true) => Ok(Some(Self { file })),
            Ok(false) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Exclusive,
    Shared,
}

/// `Ok(false)` is contention; an error is anything else the OS reported.
/// Never blocks: a caller that cannot have its turn now decides for itself
/// whether to wait, and can stop waiting.
fn lock(file: &File, mode: Mode) -> io::Result<bool> {
    let attempt = match mode {
        Mode::Exclusive => file.try_lock(),
        Mode::Shared => file.try_lock_shared(),
    };
    match attempt {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Unlocked explicitly rather than on close: a descriptor a child inherited
/// shares the lock, and would otherwise hold it for as long as it lives.
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn releasing_a_lock_does_not_wait_for_inherited_descriptors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operation.lock");
        let lock = FileLock::exclusive(&path).unwrap().unwrap();
        let inherited = lock.file.try_clone().unwrap();
        assert!(FileLock::exclusive(&path).unwrap().is_none());
        drop(lock);
        let reacquired = FileLock::exclusive(&path).unwrap();
        assert!(reacquired.is_some());
        drop(inherited);
    }
}
