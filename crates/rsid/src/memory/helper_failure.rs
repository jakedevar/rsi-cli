//! A systematic helper-LLM failure must not stay silent (#2603). Title,
//! description, memory and dream calls log a WARN per failure and fall back to
//! raw values, so a model that returns nothing on every call went unseen for
//! days. This tracker counts consecutive failures per helper purpose and, past
//! a threshold, yields one `helper_llm:<purpose>:<cause>` friction event, then
//! at most one more per renotify interval while the streak lasts. A success
//! resets the streak.

use rsi_common::friction::{FrictionKind, NewFrictionEventV1};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Consecutive failures of one purpose before the first event.
pub(crate) const HELPER_FAILURE_THRESHOLD: u32 = 10;
/// Least time between two events for one unbroken streak.
pub(crate) const HELPER_FAILURE_RENOTIFY: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
struct Streak {
    consecutive: u32,
    last_noted: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct HelperFailureTracker {
    streaks: Mutex<HashMap<String, Streak>>,
}

impl HelperFailureTracker {
    pub(crate) fn global() -> &'static Self {
        static TRACKER: OnceLock<HelperFailureTracker> = OnceLock::new();
        TRACKER.get_or_init(Self::default)
    }

    fn streaks(&self) -> std::sync::MutexGuard<'_, HashMap<String, Streak>> {
        self.streaks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn record_success(&self, purpose: &str) {
        self.streaks().remove(purpose);
    }

    /// Count one failure. `Some(consecutive)` when this failure should be
    /// surfaced: the streak reached the threshold and no event went out within
    /// the renotify interval.
    pub(crate) fn record_failure(&self, purpose: &str, now: Instant) -> Option<u32> {
        let mut streaks = self.streaks();
        let streak = streaks.entry(purpose.to_string()).or_default();
        streak.consecutive = streak.consecutive.saturating_add(1);
        if streak.consecutive < HELPER_FAILURE_THRESHOLD {
            return None;
        }
        let due = streak
            .last_noted
            .is_none_or(|noted| now.saturating_duration_since(noted) >= HELPER_FAILURE_RENOTIFY);
        if !due {
            return None;
        }
        streak.last_noted = Some(now);
        Some(streak.consecutive)
    }
}

/// The friction event for a streak: `helper_llm:<purpose>:<cause>`, where the
/// cause is `reasoning_budget` for a reply that spent its budget on reasoning
/// and `failed` otherwise. Never carries the error prose.
pub(crate) fn helper_failure_event(purpose: &str, reasoning_budget: bool) -> NewFrictionEventV1 {
    let purpose = purpose.replace(['.', '-'], "_");
    let cause = if reasoning_budget {
        "reasoning_budget"
    } else {
        "failed"
    };
    NewFrictionEventV1::new(FrictionKind::HelperLlm, &[&purpose, cause])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn a_streak_surfaces_once_at_the_threshold_and_then_once_per_interval() {
        let tracker = HelperFailureTracker::default();
        let start = Instant::now();
        for _ in 1..HELPER_FAILURE_THRESHOLD {
            assert_eq!(tracker.record_failure("session.title", start), None);
        }
        assert_eq!(
            tracker.record_failure("session.title", start),
            Some(HELPER_FAILURE_THRESHOLD)
        );
        // Still failing within the interval: silent.
        for _ in 0..50 {
            assert_eq!(tracker.record_failure("session.title", start), None);
        }
        // A different purpose has its own streak.
        assert_eq!(tracker.record_failure("session.summary", start), None);
        // Past the interval the same streak speaks again.
        assert!(
            tracker
                .record_failure("session.title", start + HELPER_FAILURE_RENOTIFY)
                .is_some()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn a_success_resets_the_streak() {
        let tracker = HelperFailureTracker::default();
        let now = Instant::now();
        for _ in 1..HELPER_FAILURE_THRESHOLD {
            tracker.record_failure("session.title", now);
        }
        tracker.record_success("session.title");
        assert_eq!(tracker.record_failure("session.title", now), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn the_event_signature_is_a_clean_code_per_purpose_and_cause() {
        assert_eq!(
            helper_failure_event("session.title", true).signature,
            "helper_llm:session_title:reasoning_budget"
        );
        assert_eq!(
            helper_failure_event("memory.observation-extract", false).signature,
            "helper_llm:memory_observation_extract:failed"
        );
    }
}
