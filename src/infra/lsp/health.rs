use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::LspManager;
use crate::models::symbol::Language;

/// Per-language failure bookkeeping for one monitor run. `consecutive` counts
/// unhealthy checks toward the next restart; `restarts` counts restart cycles
/// that have NOT yet led to recovery and is what bounds the storm — only a
/// genuine recovery clears it.
#[derive(Default)]
struct FailureTracker {
    consecutive: HashMap<Language, u32>,
    restarts: HashMap<Language, u32>,
}

pub struct HealthMonitor {
    manager: Arc<LspManager>,
    check_interval: Duration,
    failure_threshold: u32,
    /// After this many restart cycles without recovery, auto-restart is
    /// abandoned and the server is marked `CriticalFailure` — bounding the
    /// restart storm a permanently-broken server would otherwise sustain.
    max_restart_attempts: u32,
    shutdown: Arc<AtomicBool>,
}

impl HealthMonitor {
    pub fn new(manager: Arc<LspManager>) -> Self {
        Self {
            manager,
            check_interval: Duration::from_secs(30),
            failure_threshold: 3,
            max_restart_attempts: 3,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn shutdown_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.check_interval);
        let mut tracker = FailureTracker::default();

        while !self.shutdown.load(Ordering::Relaxed) {
            interval.tick().await;
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            self.check_and_recover(&mut tracker).await;
        }
        tracing::debug!("Health monitor stopped");
    }

    async fn check_and_recover(&self, tracker: &mut FailureTracker) {
        let unhealthy = self.manager.unhealthy_servers().await;
        let running = self.manager.running_languages().await;

        // Recovery: a healthy server clears all of its failure bookkeeping and
        // any prior give-up verdict.
        for lang in &running {
            if !unhealthy.contains(lang) {
                tracker.consecutive.remove(lang);
                tracker.restarts.remove(lang);
                self.manager.clear_critical_failure(*lang).await;
            }
        }

        if !self.manager.runtime_config().auto_restart {
            return;
        }

        for lang in unhealthy {
            let count = tracker.consecutive.entry(lang).or_insert(0);
            *count += 1;
            if *count < self.failure_threshold {
                continue;
            }

            let attempts = tracker.restarts.entry(lang).or_insert(0);
            if *attempts >= self.max_restart_attempts {
                // Give up: stop restarting and disclose why — but only mark and
                // log once, so a permanently-broken server neither restarts nor
                // floods the log on every subsequent tick.
                if self.manager.critical_failure_reason(lang).await.is_none() {
                    let reason = format!(
                        "language server stayed unhealthy after {} restart attempts; \
                         auto-restart abandoned — run `symora daemon restart` after fixing it",
                        attempts
                    );
                    tracing::error!("{:?} language server giving up: {}", lang, reason);
                    self.manager.mark_critical_failure(lang, reason).await;
                }
                tracker.consecutive.remove(&lang);
                continue;
            }

            *attempts += 1;
            tracing::warn!(
                "{:?} server unhealthy ({} consecutive failures), restart attempt {}/{}",
                lang,
                count,
                attempts,
                self.max_restart_attempts
            );
            // Reset the consecutive count regardless of the restart's immediate
            // outcome: we re-accumulate over the next `failure_threshold` checks
            // before restarting again, and `restarts` (cleared only on recovery)
            // is what bounds the total attempts.
            if let Err(e) = self.manager.restart_client(lang).await {
                // The restart failed to spawn/initialize, so the manager dropped
                // the client from the pool — this language will NOT reappear in
                // `unhealthy_servers` next tick, and would silently vanish
                // instead of ever reaching the give-up branch above. It already
                // failed `failure_threshold` health checks, so a failed restart
                // on top is a broken server, not a transient blip: disclose it
                // now (once) as a critical failure. Recovery still clears it when
                // a later start succeeds and the monitor sees it healthy.
                if self.manager.critical_failure_reason(lang).await.is_none() {
                    let reason =
                        format!("language server failed to start ({e}); auto-restart abandoned");
                    tracing::error!("{:?} language server giving up: {}", lang, reason);
                    self.manager.mark_critical_failure(lang, reason).await;
                }
            }
            tracker.consecutive.remove(&lang);
        }
    }
}
