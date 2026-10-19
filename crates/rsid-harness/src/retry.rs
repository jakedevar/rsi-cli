//! Per-invocation bounded retry delay; each attempt is admitted separately.
//!
//! A request that fails before any output (HTTP 429/503/5xx, a refused or reset
//! connection, a stream that dies before its first chunk) is retried inside a
//! time budget: the backoff doubles from 125 ms to [`MAX_BACKOFF`] and the waits
//! stop adding up once [`REQUEST_RETRY_BUDGET`] is spent. A stream that fails
//! after output was already forwarded is retried at most [`MAX_STREAM_RETRIES`]
//! times, and its partial output is discarded by the caller. A server
//! `Retry-After` is honoured in full up to [`MAX_RETRY_AFTER`]; a longer one is
//! not waited out, so the failure surfaces instead of retrying early (#3163).

use crate::errors::ProviderError;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Total backoff time a pre-output request failure may spend on retries.
pub const REQUEST_RETRY_BUDGET: Duration = Duration::from_secs(120);
/// Ceiling of one backoff step.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Longest `Retry-After` the loop waits out; a longer hint ends the retries.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
/// Retries of a stream that failed after output had been forwarded.
pub const MAX_STREAM_RETRIES: u32 = 5;
/// Safety bound on pre-output retries, independent of the time budget.
const MAX_REQUEST_RETRIES: u32 = 32;
const BASE_BACKOFF_MS: u64 = 125;

/// Backoff of retry number `retries_done` (0-based): doubling with jitter.
fn backoff(retries_done: u32) -> Duration {
    let floor = Duration::from_millis(BASE_BACKOFF_MS.saturating_mul(1u64 << retries_done.min(9)))
        .min(MAX_BACKOFF);
    let jitter_ms = u64::from(rand::random::<u16>())
        % u64::try_from(floor.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
    (floor + Duration::from_millis(jitter_ms)).min(MAX_BACKOFF)
}

/// What one model step has spent on retries so far.
#[derive(Debug, Clone, Default)]
pub struct RetryBudget {
    request_retries: u32,
    stream_retries: u32,
    waited: Duration,
}

impl RetryBudget {
    /// Retries taken so far; each one is a separately admitted invocation.
    pub fn attempts(&self) -> u32 {
        self.request_retries + self.stream_retries
    }

    /// How long to wait before retrying `error`, or `None` when the budget is
    /// spent, the error is not retryable or its `Retry-After` is out of bounds.
    /// `mid_stream` is true when output was already forwarded.
    pub fn next_delay(&self, error: &ProviderError, mid_stream: bool) -> Option<Duration> {
        if !error.retryable() {
            return None;
        }
        let hinted = Duration::from_millis(error.retry_after_ms.unwrap_or(0));
        if hinted > MAX_RETRY_AFTER {
            return None;
        }
        if mid_stream {
            if self.stream_retries >= MAX_STREAM_RETRIES {
                return None;
            }
            return Some(hinted.max(backoff(self.stream_retries)));
        }
        if self.request_retries >= MAX_REQUEST_RETRIES || self.waited >= REQUEST_RETRY_BUDGET {
            return None;
        }
        let remaining = REQUEST_RETRY_BUDGET - self.waited;
        Some(hinted.max(backoff(self.request_retries).min(remaining)))
    }

    /// Record a retry whose `delay` the caller is about to wait.
    pub fn record(&mut self, delay: Duration, mid_stream: bool) {
        if mid_stream {
            self.stream_retries += 1;
        } else {
            self.request_retries += 1;
        }
        self.waited += delay;
    }
}

/// Sleep for `delay`; `false` when `cancel` fired first.
pub async fn wait(delay: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! { biased; () = cancel.cancelled() => false, () = tokio::time::sleep(delay) => true }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::errors::ProviderErrorClass;

    fn error(class: ProviderErrorClass, retry_after_ms: Option<u64>) -> ProviderError {
        ProviderError {
            class,
            http_status: Some(429),
            retry_after_ms,
            detail_code: "rate_limited".into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_honours_cancellation() {
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { wait(Duration::from_secs(2), &cancel).await }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(task.await.unwrap());
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { wait(Duration::from_secs(2), &cancel).await }
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(!task.await.unwrap());
    }

    #[test]
    fn a_non_retryable_error_never_retries() {
        let budget = RetryBudget::default();
        let auth = error(ProviderErrorClass::Auth, None);
        assert!(budget.next_delay(&auth, false).is_none());
        assert!(budget.next_delay(&auth, true).is_none());
    }

    #[test]
    fn request_retries_are_sized_by_time_and_back_off_to_the_ceiling() {
        let mut budget = RetryBudget::default();
        let overloaded = error(ProviderErrorClass::Overloaded, None);
        let mut retries = 0;
        while let Some(delay) = budget.next_delay(&overloaded, false) {
            assert!(delay <= MAX_BACKOFF, "{delay:?}");
            budget.record(delay, false);
            retries += 1;
        }
        // Far more than the old four retries, and bounded by the time budget.
        assert!(retries > 4, "{retries}");
        assert!(budget.waited >= REQUEST_RETRY_BUDGET, "{:?}", budget.waited);
        assert!(budget.waited <= REQUEST_RETRY_BUDGET + MAX_BACKOFF);
    }

    #[test]
    fn a_long_retry_after_is_waited_in_full_up_to_its_bound() {
        let budget = RetryBudget::default();
        let hinted = error(ProviderErrorClass::RateLimited, Some(90_000));
        assert_eq!(
            budget.next_delay(&hinted, false),
            Some(Duration::from_secs(90))
        );
        let at_bound = error(
            ProviderErrorClass::RateLimited,
            Some(u64::try_from(MAX_RETRY_AFTER.as_millis()).unwrap()),
        );
        assert_eq!(budget.next_delay(&at_bound, false), Some(MAX_RETRY_AFTER));
        let beyond = error(
            ProviderErrorClass::RateLimited,
            Some(u64::try_from(MAX_RETRY_AFTER.as_millis()).unwrap() + 1),
        );
        assert!(budget.next_delay(&beyond, false).is_none());
    }

    #[test]
    fn stream_retries_are_bounded_by_count_not_time() {
        let mut budget = RetryBudget::default();
        let dropped = ProviderError::transient("stream_disconnected");
        for _ in 0..MAX_STREAM_RETRIES {
            let delay = budget.next_delay(&dropped, true).expect("stream retry");
            budget.record(delay, true);
        }
        assert!(budget.next_delay(&dropped, true).is_none());
        assert_eq!(budget.attempts(), MAX_STREAM_RETRIES);
        // The request budget is separate: a pre-output failure still retries.
        assert!(budget.next_delay(&dropped, false).is_some());
    }
}
