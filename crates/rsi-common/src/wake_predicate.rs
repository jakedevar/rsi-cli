//! Daemon-evaluated wait predicates for `AgentScheduleWake` mode `when` (#1006).
//!
//! A routine wait ("tell me when these jobs are done", "tell me when this
//! commit is on rolling") is a fact the daemon can check itself, so the caller
//! is woken once when the predicate is true (or its timeout passes) instead of
//! taking a model turn per event. The predicate and its bookkeeping live under
//! the `$.wake_when` key of the wake row's `schedule_json`; no table is added.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Most job ids one predicate may name.
pub const WAKE_WHEN_MAX_JOBS: usize = 32;
/// Longest `timeout_seconds` (seven days).
pub const WAKE_WHEN_MAX_TIMEOUT_SECONDS: i64 = 7 * 24 * 60 * 60;
/// Most enabled predicate wakes one session may hold.
pub const WAKE_WHEN_MAX_PER_SESSION: usize = 16;
/// The daemon re-evaluates a pending `jobs_terminal` predicate this often.
pub const WAKE_WHEN_JOBS_POLL_SECS: i64 = 5;
/// The daemon re-checks a pending `sha_on_rolling` predicate this often.
pub const WAKE_WHEN_SHA_POLL_SECS: i64 = 15;
/// Per-job bound on the refusal text carried by the wake message.
pub const WAKE_WHEN_REFUSAL_BYTES: usize = 200;
/// Bound on the whole wake message body the daemon appends.
pub const WAKE_WHEN_MESSAGE_BYTES: usize = 6_000;

/// Stable refusal codes; each is the whole `InvalidParam` text.
pub const WAKE_WHEN_PREDICATE_INVALID: &str = "wake_when_predicate_invalid";
pub const WAKE_WHEN_JOB_NOT_FOUND: &str = "wake_when_job_not_found";
pub const WAKE_WHEN_TIMEOUT_INVALID: &str = "wake_when_timeout_invalid";
pub const WAKE_WHEN_CAP_REACHED: &str = "wake_when_cap_reached";
pub const WAKE_WHEN_FIELD_MISPLACED: &str = "wake_when_field_requires_mode_when";
pub const WAKE_WHEN_TIMING_UNSUPPORTED: &str = "wake_when_uses_daemon_timing";

/// Typed reasons a predicate settles without becoming true.
pub const REASON_JOB_MISSING: &str = "job_missing";
pub const REASON_REPO_UNAVAILABLE: &str = "repo_unavailable";

/// The typed predicate: exactly one field is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WakePredicate {
    /// Every listed job (owned by the caller) is terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jobs_terminal: Option<Vec<Uuid>>,
    /// This 40-hex commit is an ancestor of `origin/rolling` in the caller's
    /// repository (as last fetched or pushed there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha_on_rolling: Option<String>,
}

/// True for a lowercase 40-hex object id.
#[must_use]
pub fn is_full_sha(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl WakePredicate {
    /// Exactly one predicate, well formed and bounded.
    ///
    /// # Errors
    /// [`WAKE_WHEN_PREDICATE_INVALID`].
    pub fn validate(&self) -> Result<(), &'static str> {
        match (&self.jobs_terminal, &self.sha_on_rolling) {
            (Some(ids), None) => {
                let mut seen = std::collections::BTreeSet::new();
                if ids.is_empty()
                    || ids.len() > WAKE_WHEN_MAX_JOBS
                    || ids.iter().any(|id| id.is_nil() || !seen.insert(*id))
                {
                    return Err(WAKE_WHEN_PREDICATE_INVALID);
                }
                Ok(())
            }
            (None, Some(sha)) if is_full_sha(sha) => Ok(()),
            _ => Err(WAKE_WHEN_PREDICATE_INVALID),
        }
    }

    /// Seconds between daemon evaluations while the predicate is pending.
    #[must_use]
    pub fn poll_seconds(&self) -> i64 {
        if self.sha_on_rolling.is_some() {
            WAKE_WHEN_SHA_POLL_SECS
        } else {
            WAKE_WHEN_JOBS_POLL_SECS
        }
    }
}

/// Validate `timeout_seconds` for a `when` wake.
///
/// # Errors
/// [`WAKE_WHEN_TIMEOUT_INVALID`].
pub fn validate_timeout(timeout_seconds: Option<i64>) -> Result<(), &'static str> {
    match timeout_seconds {
        Some(seconds) if !(1..=WAKE_WHEN_MAX_TIMEOUT_SECONDS).contains(&seconds) => {
            Err(WAKE_WHEN_TIMEOUT_INVALID)
        }
        _ => Ok(()),
    }
}

/// The persisted `$.wake_when` record of a predicate wake row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeWhenState {
    pub predicate: WakePredicate,
    pub armed_at: DateTime<Utc>,
    /// When the wake fires with `timed_out: true` if the predicate is still
    /// pending. `None` waits until the predicate settles.
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
    /// Directory of the caller's repository, for `sha_on_rolling`.
    #[serde(default)]
    pub repo_dir: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_bounded_predicate_is_valid() {
        let id = Uuid::new_v4();
        let ok = WakePredicate {
            jobs_terminal: Some(vec![id]),
            sha_on_rolling: None,
        };
        assert_eq!(ok.validate(), Ok(()));
        let sha = WakePredicate {
            jobs_terminal: None,
            sha_on_rolling: Some("a".repeat(40)),
        };
        assert_eq!(sha.validate(), Ok(()));
        for bad in [
            WakePredicate::default(),
            WakePredicate {
                jobs_terminal: Some(vec![]),
                sha_on_rolling: None,
            },
            WakePredicate {
                jobs_terminal: Some(vec![id, id]),
                sha_on_rolling: None,
            },
            WakePredicate {
                jobs_terminal: Some(vec![id]),
                sha_on_rolling: Some("a".repeat(40)),
            },
            WakePredicate {
                jobs_terminal: None,
                sha_on_rolling: Some("A".repeat(40)),
            },
            WakePredicate {
                jobs_terminal: None,
                sha_on_rolling: Some("abc".into()),
            },
            WakePredicate {
                jobs_terminal: Some((0..=WAKE_WHEN_MAX_JOBS).map(|_| Uuid::new_v4()).collect()),
                sha_on_rolling: None,
            },
        ] {
            assert_eq!(bad.validate(), Err(WAKE_WHEN_PREDICATE_INVALID), "{bad:?}");
        }
    }

    #[test]
    fn timeout_is_bounded() {
        assert_eq!(validate_timeout(None), Ok(()));
        assert_eq!(validate_timeout(Some(1)), Ok(()));
        assert_eq!(validate_timeout(Some(0)), Err(WAKE_WHEN_TIMEOUT_INVALID));
        assert_eq!(
            validate_timeout(Some(WAKE_WHEN_MAX_TIMEOUT_SECONDS + 1)),
            Err(WAKE_WHEN_TIMEOUT_INVALID)
        );
    }
}
