pub mod client;
mod params;
pub mod protocol;
pub mod server;
pub mod wire;
pub mod wire_error;

pub use client::{DaemonClient, DaemonStart};
pub use server::{DaemonRuntimeConfig, DaemonServer};

/// Whether a failed connection to the daemon socket proves nothing is
/// listening. Refusal and a missing path are that proof: the socket file
/// outlives the daemon that bound it, so a leftover path answers
/// `ConnectionRefused` and a cleaned-up one answers `NotFound`. Every
/// other failure — permissions, exhausted descriptors, a full accept
/// backlog — means the question was not answered, and reading it as an
/// absence is what turns an unreachable daemon into a second daemon: the
/// replacement unlinks the live socket, binds its own, and leaves the
/// original running with nothing able to reach it.
pub(crate) fn proves_no_listener(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    )
}

#[cfg(test)]
mod tests {
    use super::proves_no_listener;
    use std::io::{Error, ErrorKind};

    /// The whole point of the predicate: an unreachable socket is not an
    /// unbound one. Reading a permission or resource failure as an absence
    /// is what licenses unlinking a live daemon's socket.
    #[test]
    fn only_refusal_or_absence_proves_nothing_is_listening() {
        assert!(proves_no_listener(&Error::from(
            ErrorKind::ConnectionRefused
        )));
        assert!(proves_no_listener(&Error::from(ErrorKind::NotFound)));

        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::InvalidInput,
        ] {
            assert!(
                !proves_no_listener(&Error::from(kind)),
                "{kind:?} leaves the question open"
            );
        }
    }
}
