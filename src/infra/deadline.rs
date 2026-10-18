use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::error::LspError;

tokio::task_local! {
    static DEADLINE: Option<Instant>;
}

pub fn expires_at(limit: Duration) -> Option<Instant> {
    let local = Instant::now().checked_add(limit);
    let parent = DEADLINE.try_with(|current| *current).ok().flatten();
    match (local, parent) {
        (Some(local), Some(parent)) => Some(local.min(parent)),
        (local, parent) => local.or(parent),
    }
}

pub fn remaining(limit: Duration) -> Duration {
    expires_at(limit).map_or(limit, |end| end.saturating_duration_since(Instant::now()))
}

pub async fn scope<F: Future>(limit: Duration, future: F) -> F::Output {
    DEADLINE.scope(expires_at(limit), future).await
}

pub async fn run<T>(
    limit: Duration,
    future: impl Future<Output = Result<T, LspError>>,
) -> Result<T, LspError> {
    let Some(end) = expires_at(limit) else {
        return future.await;
    };
    if end <= Instant::now() {
        return Err(LspError::Timeout(
            "LSP work exhausted its shared time budget".to_string(),
        ));
    }
    tokio::time::timeout_at(end, DEADLINE.scope(Some(end), future))
        .await
        .map_err(|_| LspError::Timeout("LSP work exceeded its shared time budget".to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn nested_operations_share_the_remaining_budget() {
        let result = run(Duration::from_secs(3), async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            run(Duration::from_secs(3), async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(())
            })
            .await
        })
        .await;
        assert!(matches!(result, Err(LspError::Timeout(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_scope_prevents_new_lsp_work_without_aborting_its_owner() {
        scope(Duration::from_secs(1), async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert!(matches!(
                run(Duration::from_secs(1), async { Ok(()) }).await,
                Err(LspError::Timeout(_))
            ));
        })
        .await;
    }
}
