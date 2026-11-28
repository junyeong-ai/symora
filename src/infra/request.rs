//! The command or MCP tool call the running work answers: when it began, and
//! the facts its queries learned that its whole response states.
//!
//! A daemon serves the request's queries one RPC at a time, so each RPC is
//! sent the request's age and runs in a scope begun that long before it
//! arrived, and its response carries back what its queries learned. A fact
//! about the request then means the same in process and through a daemon.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

tokio::task_local! {
    static REQUEST: Arc<Request>;
}

struct Request {
    started: Instant,
    /// The servers, by the name they give themselves, that report no
    /// workspace readiness and were started after the request began, where
    /// one of them answered a workspace-wide query of it.
    cold_servers: Mutex<BTreeSet<String>>,
}

/// Run `future` as a request that began at `started`.
pub async fn scope<F: Future>(started: Instant, future: F) -> F::Output {
    let request = Arc::new(Request {
        started,
        cold_servers: Mutex::new(BTreeSet::new()),
    });
    REQUEST.scope(request, future).await
}

/// When the running request began, where work runs in one.
pub fn started() -> Option<Instant> {
    REQUEST.try_with(|request| request.started).ok()
}

/// How long ago the running request began, where work runs in one.
pub fn age() -> Option<Duration> {
    started().map(|started| started.elapsed())
}

/// Note that `server`, which reports no workspace readiness and was started
/// after the running request began, answered one of its workspace-wide
/// queries. Outside a request there is no response to state it in.
pub fn note_cold_server(server: &str) {
    let _ = REQUEST.try_with(|request| {
        request
            .cold_servers
            .lock()
            .expect("request lock poisoned")
            .insert(server.to_string());
    });
}

/// The servers noted by [`note_cold_server`] so far, in name order.
pub fn cold_servers() -> Vec<String> {
    REQUEST
        .try_with(|request| {
            request
                .cold_servers
                .lock()
                .expect("request lock poisoned")
                .iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_holds_its_own_facts_and_none_outside_it() {
        note_cold_server("pyright");
        assert!(cold_servers().is_empty());
        assert!(started().is_none());
        let began = Instant::now();
        let noted = scope(began, async {
            note_cold_server("pyright");
            note_cold_server("pyright");
            assert_eq!(started(), Some(began));
            cold_servers()
        })
        .await;
        assert_eq!(noted, ["pyright"]);
        assert!(
            scope(Instant::now(), async { cold_servers() })
                .await
                .is_empty()
        );
    }
}
