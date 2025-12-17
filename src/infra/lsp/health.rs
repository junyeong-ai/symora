//! LSP Server Health Monitoring

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::LspManager;
use crate::models::symbol::Language;

pub struct HealthMonitor {
    manager: Arc<LspManager>,
    check_interval: Duration,
    failure_threshold: u32,
    shutdown: Arc<AtomicBool>,
}

impl HealthMonitor {
    pub fn new(manager: Arc<LspManager>) -> Self {
        Self {
            manager,
            check_interval: Duration::from_secs(30),
            failure_threshold: 3,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn shutdown_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.check_interval);
        let mut failure_counts: HashMap<Language, u32> = HashMap::new();

        while !self.shutdown.load(Ordering::Relaxed) {
            interval.tick().await;
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            self.check_and_recover(&mut failure_counts).await;
        }
        tracing::debug!("Health monitor stopped");
    }

    async fn check_and_recover(&self, failure_counts: &mut HashMap<Language, u32>) {
        let unhealthy = self.manager.unhealthy_servers().await;
        let running = self.manager.running_languages().await;

        for lang in &running {
            if !unhealthy.contains(lang) {
                failure_counts.remove(lang);
            }
        }

        for lang in unhealthy {
            let count = failure_counts.entry(lang).or_insert(0);
            *count += 1;

            if *count >= self.failure_threshold && crate::config::auto_restart() {
                tracing::warn!(
                    "{:?} server unhealthy ({} failures), restarting",
                    lang,
                    count
                );
                if self.manager.restart_client(lang).await.is_ok() {
                    failure_counts.remove(&lang);
                }
            }
        }
    }
}
