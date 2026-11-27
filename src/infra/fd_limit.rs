//! The open-file limit of a process that serves until stopped.
//!
//! A process that answers until stopped — `daemon serve`, `mcp serve` —
//! holds every project it serves: a file watch per project, and the pipes and
//! sockets of every language server. The soft `RLIMIT_NOFILE` it inherits is
//! often far below what the hard limit allows (256 is a common macOS
//! default), and on macOS notify's FSEvents backend refuses more watch roots
//! than a twelfth of the soft limit, past which servers fall back to watching
//! files themselves. So such a process raises its soft limit to the hard one
//! at startup, as the Go runtime does for every Go program. One-shot commands
//! keep what they inherited: they watch nothing and hold one project's
//! servers for one answer.
//!
//! The raised limit is this process's own. Every process it starts that runs
//! on, or starts processes of its own, is handed the limit this process
//! inherited (`restore_for_child`): a language server runs build tools and
//! whatever a project's scripts invoke, and a program using `select()` breaks
//! on a descriptor at or above `FD_SETSIZE` (1024), which a raised limit lets
//! the kernel hand it; one that closes every descriptor up to its limit before
//! running another takes as long as that limit is high. A server that wants
//! more raises its own, as the JVM and Go servers do.
//!
//! A daemon, which outlives whoever started it, also begins by closing the
//! descriptors it inherited beyond stdio (`close_inherited`).

use std::io;
use std::process::Command;
use std::sync::OnceLock;

/// The limit this process inherited, set only once it has raised its own.
static INHERITED: OnceLock<libc::rlimit> = OnceLock::new();

/// Raise this process's soft open-file limit as far as the hard limit and
/// the platform allow. A failure leaves the inherited limit in place and is
/// reported, never fatal: the process serves as it would have otherwise.
pub fn raise() {
    let mut inherited = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes only into the struct it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut inherited) } != 0 {
        tracing::warn!(
            "could not read the open-file limit: {}",
            io::Error::last_os_error()
        );
        return;
    }
    let Some(soft) = raised_soft(inherited.rlim_cur, inherited.rlim_max, platform_cap()) else {
        tracing::debug!("open-file limit stays at {}", inherited.rlim_cur);
        return;
    };
    let raised = libc::rlimit {
        rlim_cur: soft,
        rlim_max: inherited.rlim_max,
    };
    // SAFETY: `setrlimit` only reads the struct it is given.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
        tracing::warn!(
            "could not raise the open-file limit from {} to {soft}: {}",
            inherited.rlim_cur,
            io::Error::last_os_error()
        );
        return;
    }
    let _ = INHERITED.set(inherited);
    tracing::debug!(
        "raised the open-file limit from {} to {soft}",
        inherited.rlim_cur
    );
}

/// Have `command` run under the limit this process inherited, if it raised
/// its own; otherwise it inherits this process's limit, which is that one.
pub fn restore_for_child(command: &mut Command) {
    if let Some(&inherited) = INHERITED.get() {
        run_under(command, inherited);
    }
}

fn run_under(command: &mut Command, limit: libc::rlimit) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `setrlimit` is a system call, async-signal-safe as `pre_exec`
    // requires, and the closure touches only the copy it owns. Lowering a
    // soft limit within the hard one is always permitted.
    unsafe {
        command.pre_exec(move || match libc::setrlimit(libc::RLIMIT_NOFILE, &limit) {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        });
    }
}

/// Close every descriptor above stderr. `daemon serve` calls it before it
/// opens any of its own, so all it closes are inherited: the daemon outlives
/// the command that starts it, and a descriptor that command held without
/// close-on-exec — one its own caller handed down, or a pipe another of its
/// threads had just made, which macOS marks close-on-exec only after
/// `pipe()` returns — would stay open for as long as the daemon runs, and
/// whoever reads that pipe would wait until then for its end. The starter
/// cannot close them for the child: the standard library reports a failed
/// exec through a descriptor of its own, which such a close would take.
pub fn close_inherited() {
    #[cfg(target_os = "linux")]
    // SAFETY: `close_range` takes integers alone and closes descriptors.
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) } == 0 {
        return;
    }
    // macOS has no `close_range`, nor Linux before 5.9: every descriptor
    // number this process could hold is closed in turn instead.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes only into the struct it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        tracing::warn!(
            "could not read the open-file limit to close inherited descriptors: {}",
            io::Error::last_os_error()
        );
        return;
    }
    let bound = platform_cap()
        .map_or(limit.rlim_cur, |cap| limit.rlim_cur.min(cap))
        .min(libc::c_int::MAX as libc::rlim_t) as libc::c_int;
    for fd in 3..bound {
        // SAFETY: nothing in this process refers to an inherited descriptor;
        // closing a number nothing holds fails harmlessly.
        unsafe { libc::close(fd) };
    }
}

/// The soft limit to raise to, or `None` when raising gains nothing. The
/// soft limit is never lowered, even when it is already above `cap`.
fn raised_soft(
    soft: libc::rlim_t,
    hard: libc::rlim_t,
    cap: Option<libc::rlim_t>,
) -> Option<libc::rlim_t> {
    let target = cap.map_or(hard, |cap| hard.min(cap));
    (target > soft).then_some(target)
}

/// macOS refuses a soft limit above `kern.maxfilesperproc`, and the hard
/// limit there is usually unlimited, so the hard limit alone is no target.
/// Unreadable, there is no cap, and the raise fails and is reported.
#[cfg(target_vendor = "apple")]
fn platform_cap() -> Option<libc::rlim_t> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated and `value`/`size` describe a
    // buffer of the size the sysctl returns; no new value is written.
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (read == 0 && value > 0).then_some(value as libc::rlim_t)
}

/// Elsewhere the kernel bounds the hard limit itself (Linux's `fs.nr_open`).
#[cfg(not(target_vendor = "apple"))]
fn platform_cap() -> Option<libc::rlim_t> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_soft_limit_rises_to_the_hard_one() {
        assert_eq!(raised_soft(1024, 524_288, None), Some(524_288));
    }

    #[test]
    fn an_unlimited_hard_limit_rises_only_as_far_as_the_platform_cap() {
        assert_eq!(
            raised_soft(256, libc::RLIM_INFINITY, Some(61_440)),
            Some(61_440)
        );
        assert_eq!(raised_soft(256, 10_000, Some(61_440)), Some(10_000));
    }

    #[test]
    fn a_soft_limit_already_at_the_target_or_above_it_stays() {
        assert_eq!(raised_soft(4096, 4096, None), None);
        assert_eq!(
            raised_soft(100_000, libc::RLIM_INFINITY, Some(61_440)),
            None
        );
    }

    /// A child runs under the limit it is handed, not this process's: the
    /// limit below the test process's own is what a raised process hands on.
    #[test]
    fn a_child_runs_under_the_limit_it_is_handed() {
        let mut own = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: as in `raise`.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut own) }, 0);
        let lower = own.rlim_cur.min(4096) - 1;
        let mut command = Command::new("sh");
        command.args(["-c", "ulimit -n"]);
        run_under(
            &mut command,
            libc::rlimit {
                rlim_cur: lower,
                rlim_max: own.rlim_max,
            },
        );
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            lower.to_string()
        );
    }
}
