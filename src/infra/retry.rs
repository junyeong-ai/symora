//! Retry utilities with exponential backoff

use std::future::Future;
use std::time::Duration;

use crate::error::LspError;
use crate::models::symbol::Language;

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
    pub fn for_language(language: Language) -> Self {
        if crate::config::language_profile(language).aggressive_retry {
            Self::aggressive()
        } else {
            Self::default()
        }
    }

    pub fn no_retry() -> Self {
        Self {
            max_attempts: 1,
            ..Default::default()
        }
    }

    pub fn aggressive() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(10),
            backoff_factor: 2.0,
        }
    }

    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let delay_ms = self.initial_delay.as_millis() as f64
            * self.backoff_factor.powi(attempt.saturating_sub(1) as i32);
        Duration::from_millis(delay_ms as u64).min(self.max_delay)
    }
}

/// Execute an async operation with retry and exponential backoff
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
            Err(e) if e.is_recoverable() && attempt + 1 < config.max_attempts => {
                tracing::warn!(
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

    Err(last_error.expect("Should have an error after all retries failed"))
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
                    Err(LspError::Timeout("test".to_string()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
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
