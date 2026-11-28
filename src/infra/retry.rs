use std::future::Future;
use std::time::Duration;

use crate::error::LspError;

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub backoff_factor: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
            backoff_factor: 2.0,
        }
    }
}

impl RetryConfig {
    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let delay_ms = self.initial_delay.as_millis() as f64
            * self.backoff_factor.powi(attempt.saturating_sub(1) as i32);
        Duration::from_millis(delay_ms as u64).min(self.max_delay)
    }
}

/// Execute an async operation with retry and exponential backoff. A failure
/// is tried again only while the budget the work runs under
/// ([`deadline`](crate::infra::deadline)) outlasts the wait before the next
/// attempt: past it, as after a timeout, the attempt could only fail again.
pub async fn with_retry<F, T, Fut>(config: &RetryConfig, mut op: F) -> Result<T, LspError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LspError>>,
{
    let mut last_error = None;

    for attempt in 0..config.max_attempts {
        if attempt > 0 {
            let delay = config.delay_for_attempt(attempt);
            tracing::debug!(
                "Retry attempt {}/{} after {:?}",
                attempt + 1,
                config.max_attempts,
                delay
            );
            tokio::time::sleep(delay).await;
        }

        match op().await {
            Ok(result) => return Ok(result),
            Err(e)
                if e.is_recoverable()
                    && attempt + 1 < config.max_attempts
                    && outlasts(config.delay_for_attempt(attempt + 1)) =>
            {
                tracing::debug!(
                    "Operation failed (attempt {}/{}): {}",
                    attempt + 1,
                    config.max_attempts,
                    e
                );
                last_error = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_error.unwrap_or_else(|| LspError::Protocol("No retry attempts configured".into())))
}

/// Whether the budget in force, if any, lasts past `delay` from now.
fn outlasts(delay: Duration) -> bool {
    crate::infra::deadline::current().is_none_or(|end| tokio::time::Instant::now() + delay < end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn test_successful_first_attempt() {
        let config = RetryConfig::default();
        let result = with_retry(&config, || async { Ok::<_, LspError>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_retry_on_recoverable_error() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            ..Default::default()
        };

        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let result = with_retry(&config, || {
            let c = counter_clone.clone();
            async move {
                let attempt = c.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err(LspError::timeout("test".to_string()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    /// A budget that runs out before the next attempt could start ends the
    /// retries with the failure in hand; one that lasts allows them.
    #[tokio::test(start_paused = true)]
    async fn a_failure_is_retried_only_while_the_budget_outlasts_the_wait() {
        let config = RetryConfig::default();
        for (budget, attempts) in [(Duration::from_millis(50), 1), (Duration::from_secs(5), 3)] {
            let counter = Arc::new(AtomicU32::new(0));
            let result = crate::infra::deadline::scope(
                budget,
                with_retry(&config, || {
                    let counter = Arc::clone(&counter);
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Err::<i32, _>(LspError::ServerError {
                            code: crate::infra::lsp::protocol::error_codes::CONTENT_MODIFIED,
                            message: "content modified".to_string(),
                        })
                    }
                }),
            )
            .await;
            assert!(result.is_err());
            assert_eq!(counter.load(Ordering::SeqCst), attempts, "{budget:?}");
        }
    }

    #[tokio::test]
    async fn test_no_retry_on_non_recoverable_error() {
        let config = RetryConfig::default();
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let result = with_retry(&config, || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(LspError::Protocol("not recoverable".to_string()))
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_delay_calculation() {
        let config = RetryConfig {
            max_attempts: 5,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(1000),
            backoff_factor: 2.0,
        };

        assert_eq!(config.delay_for_attempt(0), Duration::ZERO);
        assert_eq!(config.delay_for_attempt(1), Duration::from_millis(100));
        assert_eq!(config.delay_for_attempt(2), Duration::from_millis(200));
        assert_eq!(config.delay_for_attempt(3), Duration::from_millis(400));
        assert_eq!(config.delay_for_attempt(4), Duration::from_millis(800));
        assert_eq!(config.delay_for_attempt(5), Duration::from_millis(1000)); // capped
    }
}
