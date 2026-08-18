//! Advisory locks over a file, for the state two symora processes share.
//!
//! A daemon and a direct run touch the same index and the same socket, so
//! some operations need a turn rather than a merge. `flock` gives that
//! with the one property a lease cannot: the OS releases it however the
//! holder ends, so a killed process leaves nothing to time out or reclaim
//! by guesswork. Every lock is scoped to the value's lifetime.

use std::fs::{File, OpenOptions};
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
#[cfg(unix)]
fn lock(file: &File, mode: Mode) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let operation = match mode {
        Mode::Exclusive => libc::LOCK_EX,
        Mode::Shared => libc::LOCK_SH,
    };
    // SAFETY: flock with a valid fd is safe.
    if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.kind() {
        io::ErrorKind::WouldBlock => Ok(false),
        _ => Err(error),
    }
}

/// Symora ships for Unix only (see the release targets), and the state
/// these locks protect — one index, one socket — is corrupted by a second
/// writer, not merely delayed. A platform without an implementation is
/// told so rather than handed a lock that locks nothing.
#[cfg(not(unix))]
fn lock(_file: &File, _mode: Mode) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "advisory file locking is not implemented on this platform",
    ))
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
