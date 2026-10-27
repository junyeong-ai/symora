//! Workspace readiness: the states a server's structured status moves a
//! client through, and how a query is qualified by what happened while it ran.

use serde::Deserialize;
use serde_json::Value;

use crate::models::lsp::{Indexed, IndexingDegradation};

use super::{LspClient, bump_content_generation};

/// Workspace readiness reported by a healthy server's structured status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexingState {
    NotStarted,
    InProgress,
    Ready,
    Unconfirmed,
}

/// The events that may move [`IndexingState`]. Every state change goes
/// through `IndexingState::on_event` — there is no other write path — so
/// the legal transitions live in exactly one testable place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexingEvent {
    /// A quiet wait ended without a workspace status.
    WaitUnconfirmed,
    /// An explicit workspace status confirms quiescence.
    ServerQuiescent,
    /// The server reports work in progress.
    ServerBusy,
    /// The server cannot confirm a healthy workspace.
    ServerUnconfirmed,
}

impl IndexingState {
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::Unconfirmed)
    }

    pub fn degradation(self) -> Option<IndexingDegradation> {
        match self {
            Self::Ready => None,
            _ => Some(IndexingDegradation::Unconfirmed),
        }
    }

    pub fn on_event(self, event: IndexingEvent) -> IndexingState {
        use IndexingEvent::*;
        use IndexingState::*;
        match (self, event) {
            (_, ServerQuiescent) => Ready,
            (_, ServerUnconfirmed) => Unconfirmed,
            (_, ServerBusy) => InProgress,
            (NotStarted, WaitUnconfirmed) => Unconfirmed,
            (state, WaitUnconfirmed) => state,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct IndexingStatus {
    pub(super) state: IndexingState,
    pub(super) revision: u64,
    /// Whether the server has reported a workspace status at all.
    pub(super) reported: bool,
}

impl IndexingStatus {
    /// Whether the server reports its workspace is not ready to answer from.
    pub(super) fn reports_not_ready(&self) -> bool {
        self.reported && self.state != IndexingState::Ready
    }
}

/// Qualifies a result across the request, including a complete busy/ready cycle.
#[must_use]
pub struct IndexingQuery<'a> {
    client: &'a LspClient,
    initial: IndexingStatus,
    degradation: Option<IndexingDegradation>,
}

impl IndexingQuery<'_> {
    pub fn finish<T>(self, data: T) -> Indexed<T> {
        let current = self.client.indexing_status();
        let indexing = self.degradation.or_else(|| {
            (self.initial.revision != current.revision).then_some(IndexingDegradation::Unconfirmed)
        });
        Indexed::new(data, indexing)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ServerHealth {
    Ok,
    Warning,
    Error,
}

#[derive(Deserialize)]
struct ServerStatus {
    health: ServerHealth,
    quiescent: bool,
}

impl LspClient {
    pub(super) fn indexing_status(&self) -> IndexingStatus {
        *self
            .indexing_status
            .lock()
            .expect("indexing status lock poisoned")
    }

    pub fn indexing_state(&self) -> IndexingState {
        self.indexing_status().state
    }

    pub fn indexing_query(&self, waited: Option<IndexingDegradation>) -> IndexingQuery<'_> {
        let initial = self.indexing_status();
        let degradation = if initial.state == IndexingState::Ready {
            None
        } else {
            waited.or(initial.state.degradation())
        };
        IndexingQuery {
            client: self,
            initial,
            degradation,
        }
    }

    pub fn apply_indexing_event(&self, event: IndexingEvent) -> IndexingState {
        let mut status = self
            .indexing_status
            .lock()
            .expect("indexing status lock poisoned");
        let previous = status.state;
        let next = previous.on_event(event);
        if event != IndexingEvent::WaitUnconfirmed {
            status.reported = true;
        }
        if matches!(
            event,
            IndexingEvent::ServerBusy | IndexingEvent::ServerUnconfirmed
        ) {
            status.revision += 1;
        }
        if previous == IndexingState::Ready && next != IndexingState::Ready {
            bump_content_generation();
        }
        status.state = next;
        drop(status);
        if next != previous && next.is_usable() {
            self.indexing_notify.notify_waiters();
        }
        next
    }

    #[cfg(test)]
    pub(super) fn force_indexing_state(&self, state: IndexingState) {
        self.indexing_status.lock().unwrap().state = state;
    }

    /// Invalidate workspace answers when source content changes. Readiness
    /// remains signal-driven; elapsed time does not confirm indexing.
    pub fn note_document_changed(&self) {
        bump_content_generation();
    }

    pub async fn await_indexing_signal(&self) -> Option<IndexingDegradation> {
        let state = self.indexing_state();
        if state.is_usable() {
            return state.degradation();
        }
        let wait_limit = self.config.indexing_wait(self.language);
        let policy_deadline = tokio::time::Instant::now() + wait_limit;
        let deadline = crate::infra::deadline::expires_at(wait_limit)
            .unwrap_or(policy_deadline)
            .min(policy_deadline);
        let quiet_wait = deadline == policy_deadline;
        loop {
            let notified = self.indexing_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let current = self.indexing_state();
            if current.is_usable() {
                return current.degradation();
            }
            tokio::select! {
                _ = &mut notified => {},
                _ = tokio::time::sleep_until(deadline) => {
                    let current = if quiet_wait {
                        self.apply_indexing_event(IndexingEvent::WaitUnconfirmed)
                    } else {
                        self.indexing_state()
                    };
                    return if current.is_usable() {
                        current.degradation()
                    } else {
                        Some(IndexingDegradation::TimedOut)
                    };
                }
            }
        }
    }

    pub(super) fn observe_server_status(&self, params: &Value) {
        let Ok(status) = serde_json::from_value::<ServerStatus>(params.clone()) else {
            return;
        };
        let event = if !status.quiescent {
            IndexingEvent::ServerBusy
        } else if matches!(status.health, ServerHealth::Ok) {
            IndexingEvent::ServerQuiescent
        } else {
            IndexingEvent::ServerUnconfirmed
        };
        self.apply_indexing_event(event);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{notify_client, test_client};
    use super::*;
    use crate::models::symbol::Language;
    use std::sync::Arc;
    use std::time::Duration;

    fn progress(token: &str, kind: &str, title: Option<&str>) -> serde_json::Value {
        let mut value = serde_json::json!({ "kind": kind });
        if let Some(title) = title {
            value["title"] = serde_json::Value::String(title.to_string());
        }
        serde_json::json!({ "token": token, "value": value })
    }

    /// `experimental/serverStatus` drives readiness in BOTH directions:
    /// quiescent=true is Ready, false re-opens InProgress — the state is
    /// re-evaluated per signal, never latched on a one-shot verdict.
    #[tokio::test]
    async fn server_status_drives_readiness_in_both_directions() {
        let client = test_client();

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"health": "ok", "quiescent": true}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::Ready);

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"health": "ok", "quiescent": false}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::InProgress);

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"health": "ok", "quiescent": true}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::Ready);
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_wait_does_not_end_another_callers_wait() {
        let client = test_client();
        client.apply_indexing_event(IndexingEvent::ServerBusy);
        let patient = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.await_indexing_signal().await }
        });
        let start = tokio::time::Instant::now();
        let short = crate::infra::deadline::scope(Duration::from_secs(1), async {
            tokio::time::sleep(Duration::from_millis(750)).await;
            client.await_indexing_signal().await
        })
        .await;
        assert_eq!(short, Some(IndexingDegradation::TimedOut));
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(1));
        assert_eq!(client.indexing_state(), IndexingState::InProgress);
        assert!(!patient.is_finished());
        client.apply_indexing_event(IndexingEvent::ServerQuiescent);
        assert_eq!(patient.await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_and_expired_waits_preserve_server_progress() {
        let client = test_client();
        client.apply_indexing_event(IndexingEvent::ServerBusy);
        let cancelled = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.await_indexing_signal().await }
        });
        tokio::task::yield_now().await;
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        assert_eq!(client.indexing_state(), IndexingState::InProgress);
        let waited = client.await_indexing_signal().await;
        assert_eq!(waited, Some(IndexingDegradation::TimedOut));
        assert_eq!(client.indexing_query(waited).finish(()).indexing, waited);
        assert_eq!(client.indexing_state(), IndexingState::InProgress);
        client.apply_indexing_event(IndexingEvent::ServerQuiescent);
        assert_eq!(client.await_indexing_signal().await, None);
        assert!(client.indexing_query(None).finish(()).indexing.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_server_stays_unconfirmed_until_it_reports_readiness() {
        let client = test_client();
        let started = tokio::time::Instant::now();
        assert_eq!(
            client.await_indexing_signal().await,
            Some(IndexingDegradation::Unconfirmed)
        );
        let after_wait = tokio::time::Instant::now();
        assert_eq!(
            after_wait - started,
            client.config.indexing_wait(Language::Rust)
        );
        assert_eq!(
            client.await_indexing_signal().await,
            Some(IndexingDegradation::Unconfirmed)
        );
        assert_eq!(tokio::time::Instant::now(), after_wait);
        client.apply_indexing_event(IndexingEvent::ServerQuiescent);
        assert_eq!(client.await_indexing_signal().await, None);
    }

    #[tokio::test]
    async fn quiescence_requires_a_valid_healthy_server_status() {
        let client = test_client();
        for params in [
            serde_json::json!({"quiescent": true}),
            serde_json::json!({"health": "unknown", "quiescent": true}),
            serde_json::json!({"health": "ok", "quiescent": "true"}),
        ] {
            notify_client(&client, "experimental/serverStatus", params).await;
            assert_eq!(client.indexing_state(), IndexingState::NotStarted);
        }
        for health in ["warning", "error"] {
            client.apply_indexing_event(IndexingEvent::ServerQuiescent);
            notify_client(
                &client,
                "experimental/serverStatus",
                serde_json::json!({"health": health, "quiescent": true}),
            )
            .await;
            assert_eq!(client.indexing_state(), IndexingState::Unconfirmed);
        }
    }

    #[tokio::test]
    async fn project_health_and_document_diagnostics_do_not_confirm_workspace_readiness() {
        let client = test_client();
        notify_client(
            &client,
            "language/status",
            serde_json::json!({"type": "ProjectStatus", "message": "OK"}),
        )
        .await;
        notify_client(
            &client,
            "textDocument/publishDiagnostics",
            serde_json::json!({"uri": "file:///tmp/other.py", "diagnostics": []}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::NotStarted);
    }

    #[test]
    fn query_readiness_covers_the_entire_computation() {
        let client = test_client();
        let unconfirmed = client.indexing_query(None);
        client.apply_indexing_event(IndexingEvent::ServerQuiescent);
        assert_eq!(
            unconfirmed.finish(()).indexing,
            Some(IndexingDegradation::Unconfirmed)
        );
        assert!(client.indexing_query(None).finish(()).indexing.is_none());

        let interrupted = client.indexing_query(None);
        client.apply_indexing_event(IndexingEvent::ServerBusy);
        client.apply_indexing_event(IndexingEvent::ServerQuiescent);
        assert_eq!(
            interrupted.finish(()).indexing,
            Some(IndexingDegradation::Unconfirmed)
        );
        assert!(client.indexing_query(None).finish(()).indexing.is_none());

        let unhealthy = client.indexing_query(None);
        client.apply_indexing_event(IndexingEvent::ServerUnconfirmed);
        client.apply_indexing_event(IndexingEvent::ServerQuiescent);
        assert_eq!(
            unhealthy.finish(()).indexing,
            Some(IndexingDegradation::Unconfirmed)
        );
    }

    #[tokio::test]
    async fn human_readable_logs_do_not_establish_readiness() {
        let client = test_client();
        for message in ["Found 200 source files", "Loading completed", "Initialized"] {
            notify_client(
                &client,
                "window/logMessage",
                serde_json::json!({"type": 3, "message": message}),
            )
            .await;
            assert_eq!(client.indexing_state(), IndexingState::NotStarted);
        }
    }

    #[test]
    fn indexing_transition_table_is_exact() {
        use IndexingEvent::*;
        use IndexingState::*;
        for state in [NotStarted, InProgress, Ready, Unconfirmed] {
            assert_eq!(state.on_event(ServerQuiescent), Ready);
            assert_eq!(state.on_event(ServerUnconfirmed), Unconfirmed);
            assert_eq!(state.on_event(ServerBusy), InProgress);
            assert_eq!(
                state.on_event(WaitUnconfirmed),
                if state == NotStarted {
                    Unconfirmed
                } else {
                    state
                }
            );
        }
    }

    #[tokio::test]
    async fn waiting_preserves_an_existing_readiness_confirmation() {
        let client = test_client();
        client.apply_indexing_event(IndexingEvent::ServerQuiescent);
        assert_eq!(client.await_indexing_signal().await, None);
        // And again — quiescence is durable across repeated waits.
        assert_eq!(client.await_indexing_signal().await, None);
    }

    #[tokio::test]
    async fn progress_does_not_claim_workspace_readiness() {
        let client = test_client();
        for title in ["Indexing", "Loading workspace", "Analyze selection"] {
            notify_client(
                &client,
                "$/progress",
                progress("task", "begin", Some(title)),
            )
            .await;
            notify_client(&client, "$/progress", progress("task", "end", None)).await;
            assert_eq!(client.indexing_state(), IndexingState::NotStarted);
        }
        client.apply_indexing_event(IndexingEvent::ServerBusy);
        notify_client(&client, "$/progress", progress("task", "end", None)).await;
        assert_eq!(client.indexing_state(), IndexingState::InProgress);
    }

    /// An edit does not unprove a server's demonstrated quiescence: the
    /// state stays Ready (servers that genuinely re-index report busy via
    /// their status channel), so the post-edit checked-delete workflow
    /// never starves on a signal that will never re-fire.
    #[tokio::test]
    async fn document_change_does_not_unprove_readiness() {
        let client = test_client();
        client.force_indexing_state(IndexingState::Ready);
        client.note_document_changed();
        assert_eq!(client.indexing_state(), IndexingState::Ready);

        client.force_indexing_state(IndexingState::InProgress);
        client.note_document_changed();
        assert_eq!(client.indexing_state(), IndexingState::InProgress);
    }
}
