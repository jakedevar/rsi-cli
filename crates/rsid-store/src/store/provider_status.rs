//! `AgentGetProviderStatus` (#1044): recent launch and provider-error facts
//! derived from rows the daemon already writes (`sessions.provider`,
//! `sessions.stop_reason = 'provider_error:...'`). No schema change.
//!
//! A session's failure time is approximated as `created_at + duration_ms`
//! (the row keeps no separate end timestamp; `updated_at` moves on archive).

use super::Store;
use crate::error::Result;
use chrono::{DateTime, Duration, Utc};
use rsi_common::agent_provider_status::{
    PROVIDER_STATUS_FAILURE_WINDOW_SECS, PROVIDER_STATUS_LAST_ERROR_LOOKBACK_SECS,
};
use std::collections::HashMap;

/// Cap on the rows scanned per query, so a busy database cannot make the read
/// unbounded.
const MAX_SCAN_ROWS: i64 = 20_000;
const MAX_ERROR_ROWS: i64 = 1_000;

/// Coarse class of a `provider_error:` stop reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderErrorKind {
    /// Credit or quota exhausted (402-class).
    Credit,
    /// Rate limited (429-class).
    RateLimit,
    /// The provider rejected the credential (401-class, #1610).
    Auth,
}

/// Classify a `stop_reason` such as `provider_error:credit_exhausted`.
pub(crate) fn classify_stop_reason(reason: &str) -> Option<ProviderErrorKind> {
    let lower = reason.to_ascii_lowercase();
    if !lower.starts_with("provider_error:") {
        return None;
    }
    let has = |terms: &[&str]| terms.iter().any(|term| lower.contains(term));
    if crate::store_support::provider_defaults::is_provider_auth_failure_text(&lower) {
        Some(ProviderErrorKind::Auth)
    } else if has(&[
        "credit_exhausted",
        "402",
        "payment required",
        "insufficient",
        "usage limit",
        "usage_limit",
        "out of credits",
    ]) {
        Some(ProviderErrorKind::Credit)
    } else if has(&["rate_limited", "429", "rate limit", "too many requests"]) {
        Some(ProviderErrorKind::RateLimit)
    } else {
        None
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderLaunchStats {
    pub launches: u32,
    pub failed: u32,
    pub last_credit_error_at: Option<DateTime<Utc>>,
    pub last_rate_limit_at: Option<DateTime<Utc>>,
    /// Last auth-rejected (401-class) startup inside the lookback.
    pub last_auth_failure_at: Option<DateTime<Utc>>,
    /// First auth failure after the latest launch that got past startup; the
    /// start of the current auth-failure episode. `None` once a later launch
    /// succeeded.
    pub auth_episode_started_at: Option<DateTime<Utc>>,
}

/// A launch that is still running this long without a provider error got past
/// startup, where an auth rejection surfaces.
const AUTH_SUCCESS_MIN_AGE_SECS: i64 = 120;

fn day_prefix(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

fn parse(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

impl Store {
    /// Per-provider (serde `SessionProvider` string) launch counters for the
    /// last 24 h and the last credit/rate-limit failure inside the 7-day
    /// lookback. Containers (Group/Epic) never launch and are excluded.
    pub fn provider_launch_stats(
        &self,
        now: DateTime<Utc>,
    ) -> Result<HashMap<String, ProviderLaunchStats>> {
        let window_start = now - Duration::seconds(PROVIDER_STATUS_FAILURE_WINDOW_SECS);
        let lookback_start = now - Duration::seconds(PROVIDER_STATUS_LAST_ERROR_LOOKBACK_SECS);
        let mut stats: HashMap<String, ProviderLaunchStats> = HashMap::new();

        // The day prefix compares lexically regardless of the `Z` / `+00:00`
        // suffix; the exact window is applied after parsing.
        let mut launches = self.conn.prepare(
            "SELECT provider, created_at, stop_reason LIKE 'provider_error:%', status
             FROM sessions
             WHERE created_at >= ?1 AND COALESCE(session_kind,'') NOT IN ('Group','Epic')
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = launches.query_map(
            rusqlite::params![day_prefix(window_start), MAX_SCAN_ROWS],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<bool>>(2)?.unwrap_or(false),
                    row.get::<_, String>(3)?,
                ))
            },
        )?;
        let mut last_success: HashMap<String, DateTime<Utc>> = HashMap::new();
        for row in rows {
            let (provider, created_at, failed, status) = row?;
            let Some(created) = parse(&created_at) else {
                continue;
            };
            if created < window_start || created > now {
                continue;
            }
            let got_past_startup = !failed
                && match status.as_str() {
                    "Completed" | "Archived" => true,
                    "Running" | "WaitingApproval" => {
                        now.signed_duration_since(created)
                            >= Duration::seconds(AUTH_SUCCESS_MIN_AGE_SECS)
                    }
                    _ => false,
                };
            if got_past_startup {
                let slot = last_success.entry(provider.clone()).or_insert(created);
                *slot = (*slot).max(created);
            }
            let entry = stats.entry(provider).or_default();
            entry.launches = entry.launches.saturating_add(1);
            if failed {
                entry.failed = entry.failed.saturating_add(1);
            }
        }

        let mut errors = self.conn.prepare(
            "SELECT provider, stop_reason, created_at, duration_ms
             FROM sessions
             WHERE created_at >= ?1 AND stop_reason LIKE 'provider_error:%'
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = errors.query_map(
            rusqlite::params![day_prefix(lookback_start), MAX_ERROR_ROWS],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )?;
        let mut auth_failures: Vec<(String, DateTime<Utc>)> = Vec::new();
        for row in rows {
            let (provider, reason, created_at, duration_ms) = row?;
            let (Some(kind), Some(created)) = (classify_stop_reason(&reason), parse(&created_at))
            else {
                continue;
            };
            let at = (created + Duration::milliseconds(duration_ms.unwrap_or(0).max(0))).min(now);
            if at < lookback_start {
                continue;
            }
            if kind == ProviderErrorKind::Auth {
                auth_failures.push((provider.clone(), at));
            }
            let entry = stats.entry(provider).or_default();
            let slot = match kind {
                ProviderErrorKind::Credit => &mut entry.last_credit_error_at,
                ProviderErrorKind::RateLimit => &mut entry.last_rate_limit_at,
                ProviderErrorKind::Auth => &mut entry.last_auth_failure_at,
            };
            if slot.is_none_or(|previous| at > previous) {
                *slot = Some(at);
            }
        }
        // The episode starts at the earliest auth failure after the latest
        // launch that got past startup (the failed launch itself never counts).
        for (provider, at) in auth_failures {
            if last_success.get(&provider).is_some_and(|ok| *ok > at) {
                continue;
            }
            if let Some(entry) = stats.get_mut(&provider)
                && entry.auth_episode_started_at.is_none_or(|first| at < first)
            {
                entry.auth_episode_started_at = Some(at);
            }
        }
        Ok(stats)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn stop_reasons_classify_credit_and_rate_limit() {
        use ProviderErrorKind::{Credit, RateLimit};
        assert_eq!(
            classify_stop_reason("provider_error:credit_exhausted"),
            Some(Credit)
        );
        assert_eq!(
            classify_stop_reason("provider_error:codex:unexpected status 402 Payment Required"),
            Some(Credit)
        );
        assert_eq!(
            classify_stop_reason("provider_error:codex_usage_limit"),
            Some(Credit)
        );
        assert_eq!(
            classify_stop_reason("provider_error:rate_limited"),
            Some(RateLimit)
        );
        assert_eq!(classify_stop_reason("provider_error:unclassified"), None);
        assert_eq!(classify_stop_reason("credit_exhausted"), None);
    }

    fn insert(store: &Store, id: &str, provider: &str, kind: &str, stop: Option<&str>, at: &str) {
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, provider, query, working_dir, status, created_at,
                                       updated_at, stop_reason, session_kind, duration_ms)
                 VALUES (?1, ?2, 'q', '/tmp', 'Failed', ?3, ?3, ?4, ?5, 2000)",
                rusqlite::params![id, provider, at, stop, kind],
            )
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn auth_rejection_opens_an_episode_that_a_later_success_closes() {
        assert_eq!(
            classify_stop_reason(
                "provider_error:codex:workspace routing discovery unauthorized (401)"
            ),
            Some(ProviderErrorKind::Auth)
        );
        assert_eq!(
            classify_stop_reason(
                crate::store_support::provider_defaults::PROVIDER_AUTH_INVALID_STOP_REASON
            ),
            Some(ProviderErrorKind::Auth)
        );
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let ts = |mins: i64| (now - Duration::minutes(mins)).to_rfc3339();
        let auth = "provider_error:provider_auth_invalid";
        insert(
            &store,
            "00000000-0000-4000-8000-000000000011",
            "Codex",
            "Task",
            Some(auth),
            &ts(30),
        );
        insert(
            &store,
            "00000000-0000-4000-8000-000000000012",
            "Codex",
            "Task",
            Some(auth),
            &ts(20),
        );
        let stats = store.provider_launch_stats(now).unwrap();
        let codex = &stats["Codex"];
        assert!(codex.last_auth_failure_at.is_some());
        assert!(
            codex
                .auth_episode_started_at
                .is_some_and(|start| start < codex.last_auth_failure_at.unwrap()),
            "the episode starts at the first failure"
        );
        // A later Completed Codex session proves the credential works again.
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, provider, query, working_dir, status, created_at,
                                       updated_at, session_kind, duration_ms)
                 VALUES ('00000000-0000-4000-8000-000000000013','Codex','q','/tmp','Completed',
                         ?1,?1,'Task',2000)",
                [ts(5)],
            )
            .unwrap();
        let stats = store.provider_launch_stats(now).unwrap();
        let codex = &stats["Codex"];
        assert!(codex.last_auth_failure_at.is_some());
        assert_eq!(codex.auth_episode_started_at, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn launch_stats_count_window_and_track_last_errors() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let ts = |mins: i64| (now - Duration::minutes(mins)).to_rfc3339();
        insert(
            &store,
            "00000000-0000-4000-8000-000000000001",
            "OpenRouter",
            "Task",
            Some("provider_error:credit_exhausted"),
            &ts(30),
        );
        insert(
            &store,
            "00000000-0000-4000-8000-000000000002",
            "OpenRouter",
            "Task",
            Some("provider_error:rate_limited"),
            &ts(20),
        );
        insert(
            &store,
            "00000000-0000-4000-8000-000000000003",
            "OpenRouter",
            "Task",
            None,
            &ts(10),
        );
        insert(
            &store,
            "00000000-0000-4000-8000-000000000004",
            "OpenRouter",
            "Epic",
            None,
            &ts(10),
        );
        // Older than 24 h but inside the 7-day lookback: no launch count, but
        // it still sets the last-402 time.
        insert(
            &store,
            "00000000-0000-4000-8000-000000000005",
            "Bedrock",
            "Task",
            Some("provider_error:codex:unexpected status 402"),
            &ts(60 * 48),
        );
        let stats = store.provider_launch_stats(now).unwrap();
        let or = &stats["OpenRouter"];
        assert_eq!((or.launches, or.failed), (3, 2));
        assert!(or.last_credit_error_at.is_some());
        assert!(
            or.last_rate_limit_at
                .is_some_and(|at| at > or.last_credit_error_at.unwrap())
        );
        let bedrock = &stats["Bedrock"];
        assert_eq!(bedrock.launches, 0);
        assert!(bedrock.last_credit_error_at.is_some());
    }
}
