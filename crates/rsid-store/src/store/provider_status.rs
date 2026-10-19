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
    /// The provider CLI holds no login (#2753): the host must log in. It is a
    /// 401-class failure for `last_auth_failure_at`, but opens no auth-failure
    /// episode: the fix is the login, which the login probe tracks.
    Login,
}

/// Classify a `stop_reason` such as `provider_error:credit_exhausted`.
pub(crate) fn classify_stop_reason(reason: &str) -> Option<ProviderErrorKind> {
    let lower = reason.to_ascii_lowercase();
    if !lower.starts_with("provider_error:") {
        return None;
    }
    let has = |terms: &[&str]| terms.iter().any(|term| lower.contains(term));
    if crate::store_support::provider_defaults::is_provider_login_required_text(&lower) {
        Some(ProviderErrorKind::Login)
    } else if crate::store_support::provider_defaults::is_provider_auth_failure_text(&lower) {
        Some(ProviderErrorKind::Auth)
    } else if has(&[
        "credit_exhausted",
        "billing_error",
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

impl ProviderLaunchStats {
    /// The open auth-failure episode as the launch history alone shows it at
    /// `now` (hub #2957): refused until a probe interval after the last
    /// failure, then admitted. The CLI login reading is folded in separately.
    #[must_use]
    pub fn auth_episode(
        &self,
        now: DateTime<Utc>,
    ) -> Option<rsi_common::agent_provider_status::ProviderAuthEpisodeV1> {
        let (started, last) = self
            .auth_episode_started_at
            .zip(self.last_auth_failure_at)?;
        Some(
            rsi_common::agent_provider_status::ProviderAuthEpisodeV1::from_failures(
                started, last, now,
            ),
        )
    }
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
                ProviderErrorKind::Auth | ProviderErrorKind::Login => {
                    &mut entry.last_auth_failure_at
                }
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

/// A model the provider refused as unsupported or unavailable (#2735), derived
/// from the sessions that died on that refusal. No schema: the evidence is the
/// session rows' typed `provider_error:model_unavailable:` stop reason (or the
/// Claude `provider_model_unrecognized` one) and their `model`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ModelUnavailable {
    /// Canonical `SessionProvider` spelling of the refused session (`Codex`).
    pub provider: String,
    pub model: String,
    /// The first refusal of the current episode.
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// The refused session that opened the episode.
    pub evidence_session_id: String,
    pub evidence_stop_reason: String,
}

/// Providers that share one model catalog (and so one refusal).
fn provider_catalog_key(provider: &str) -> &str {
    match provider {
        "CodexAppServer" => "Codex",
        other => other,
    }
}

impl Store {
    /// Every model currently unavailable: refused inside
    /// [`MODEL_UNAVAILABLE_TTL_SECS`](crate::store_support::provider_defaults::MODEL_UNAVAILABLE_TTL_SECS)
    /// with no later launch of the same provider and model that got past
    /// startup. Oldest episode first; bounded by the scan caps.
    pub fn models_unavailable(&self, now: DateTime<Utc>) -> Result<Vec<ModelUnavailable>> {
        use crate::store_support::provider_defaults::{
            MODEL_UNAVAILABLE_TTL_SECS, model_unavailable_stop_reason_model,
        };
        let window_start = now - Duration::seconds(MODEL_UNAVAILABLE_TTL_SECS);
        let mut failures = self.conn.prepare(
            "SELECT id, provider, model, stop_reason, created_at, duration_ms
             FROM sessions
             WHERE created_at >= ?1
               AND (stop_reason LIKE 'provider_error:model_unavailable:%'
                    OR stop_reason = 'provider_model_unrecognized')
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = failures.query_map(
            rusqlite::params![day_prefix(window_start), MAX_ERROR_ROWS],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            },
        )?;
        // (provider catalog, model) -> (first_seen, last_seen, evidence)
        let mut episodes: HashMap<(String, String), ModelUnavailable> = HashMap::new();
        for row in rows {
            let (id, provider, session_model, reason, created_at, duration_ms) = row?;
            let (Some(named), Some(created)) = (
                model_unavailable_stop_reason_model(&reason),
                parse(&created_at),
            ) else {
                continue;
            };
            let Some(model) = named.or(session_model).filter(|model| !model.is_empty()) else {
                continue;
            };
            let at = (created + Duration::milliseconds(duration_ms.unwrap_or(0).max(0))).min(now);
            if at < window_start {
                continue;
            }
            let provider = provider_catalog_key(&provider).to_string();
            let entry = episodes
                .entry((provider.clone(), model.clone()))
                .or_insert_with(|| ModelUnavailable {
                    provider,
                    model,
                    first_seen: at,
                    last_seen: at,
                    evidence_session_id: id.clone(),
                    evidence_stop_reason: reason.clone(),
                });
            if at < entry.first_seen {
                entry.first_seen = at;
                entry.evidence_session_id = id;
                entry.evidence_stop_reason = reason;
            }
            entry.last_seen = entry.last_seen.max(at);
        }
        let mut out = Vec::new();
        for (_, episode) in episodes {
            if !self.model_launch_got_past_startup_since(&episode, now)? {
                out.push(episode);
            }
        }
        out.sort_by(|a, b| {
            (a.first_seen, &a.provider, &a.model).cmp(&(b.first_seen, &b.provider, &b.model))
        });
        Ok(out)
    }

    /// Whether a launch of the episode's provider and model started after its
    /// last refusal and got past startup, which proves the model works again.
    fn model_launch_got_past_startup_since(
        &self,
        episode: &ModelUnavailable,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let mut launches = self.conn.prepare(
            "SELECT created_at, status, provider FROM sessions
             WHERE model = ?1 AND created_at >= ?2
               AND (stop_reason IS NULL OR stop_reason NOT LIKE 'provider_%')
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = launches.query_map(
            rusqlite::params![episode.model, day_prefix(episode.last_seen), MAX_ERROR_ROWS],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        for row in rows {
            let (created_at, status, provider) = row?;
            let Some(created) = parse(&created_at) else {
                continue;
            };
            if provider_catalog_key(&provider) != episode.provider || created <= episode.last_seen {
                continue;
            }
            let past = match status.as_str() {
                "Completed" | "Archived" => true,
                "Running" | "WaitingApproval" => {
                    now.signed_duration_since(created)
                        >= Duration::seconds(AUTH_SUCCESS_MIN_AGE_SECS)
                }
                _ => false,
            };
            if past {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The unavailable record for one provider and model, if any. A `Codex`
    /// app-server session shares the Codex catalog.
    pub fn model_unavailable(
        &self,
        provider: &str,
        model: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<ModelUnavailable>> {
        let provider = provider_catalog_key(provider);
        Ok(self
            .models_unavailable(now)?
            .into_iter()
            .find(|episode| episode.provider == provider && episode.model == model))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn login_and_billing_stop_reasons_have_their_own_classes() {
        use crate::store_support::provider_defaults::{
            PROVIDER_BILLING_ERROR_STOP_REASON, PROVIDER_LOGIN_REQUIRED_STOP_REASON,
        };
        assert_eq!(
            classify_stop_reason(PROVIDER_LOGIN_REQUIRED_STOP_REASON),
            Some(ProviderErrorKind::Login)
        );
        assert_eq!(
            classify_stop_reason(PROVIDER_BILLING_ERROR_STOP_REASON),
            Some(ProviderErrorKind::Credit)
        );
        // A Codex logged-out text riding a 401 is a login, not a credential.
        assert_eq!(
            classify_stop_reason(
                "provider_error:codex:unexpected status 401 Unauthorized: Missing bearer or basic authentication"
            ),
            Some(ProviderErrorKind::Login)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_login_failure_sets_the_auth_time_but_opens_no_auth_episode() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        insert(
            &store,
            "00000000-0000-4000-8000-000000000041",
            "Claude",
            "Task",
            Some(crate::store_support::provider_defaults::PROVIDER_LOGIN_REQUIRED_STOP_REASON),
            &(now - Duration::minutes(10)).to_rfc3339(),
        );
        let stats = store.provider_launch_stats(now).unwrap();
        let claude = &stats["Claude"];
        assert!(claude.last_auth_failure_at.is_some());
        assert_eq!(claude.auth_episode_started_at, None);
        assert_eq!((claude.launches, claude.failed), (1, 1));
    }

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
    fn an_auth_episode_admits_a_probe_after_the_bound_with_no_success_between() {
        use rsi_common::agent_provider_status::{
            PROVIDER_AUTH_PROBE_INTERVAL_SECS, ProviderAuthEpisodeStateV1,
        };
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let failed_at = now - Duration::minutes(10);
        insert(
            &store,
            "00000000-0000-4000-8000-000000000051",
            "Codex",
            "Task",
            Some("provider_error:provider_auth_invalid"),
            &failed_at.to_rfc3339(),
        );
        let stats = store.provider_launch_stats(now).unwrap();
        let codex = &stats["Codex"];
        let episode = codex.auth_episode(now).unwrap();
        assert!(episode.refuses());
        let probe_at = episode.next_probe_at.unwrap();
        assert_eq!(
            probe_at,
            codex.last_auth_failure_at.unwrap()
                + Duration::seconds(PROVIDER_AUTH_PROBE_INTERVAL_SECS)
        );
        // Still no launch that got past startup: the episode is open, and one
        // probe is admitted once the bound has passed.
        let later = probe_at + Duration::seconds(1);
        let stats = store.provider_launch_stats(later).unwrap();
        let episode = stats["Codex"].auth_episode(later).unwrap();
        assert_eq!(episode.state, ProviderAuthEpisodeStateV1::ProbeAdmitted);
        assert!(!episode.refuses());
        assert_eq!(
            episode.started_at,
            stats["Codex"].auth_episode_started_at.unwrap()
        );
        // A failed probe is a new last failure and refuses for another bound.
        insert(
            &store,
            "00000000-0000-4000-8000-000000000052",
            "Codex",
            "Task",
            Some("provider_error:provider_auth_invalid"),
            &(later - Duration::seconds(30)).to_rfc3339(),
        );
        let stats = store.provider_launch_stats(later).unwrap();
        let episode = stats["Codex"].auth_episode(later).unwrap();
        assert!(episode.refuses());
        assert_eq!(
            episode.started_at,
            stats["Codex"].auth_episode_started_at.unwrap()
        );
        assert!(episode.last_failure_at > failed_at + Duration::minutes(50));
    }

    fn insert_model(
        store: &Store,
        id: &str,
        provider: &str,
        model: &str,
        status: &str,
        stop: Option<&str>,
        at: &str,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, provider, model, query, working_dir, status,
                                       created_at, updated_at, stop_reason, session_kind,
                                       duration_ms)
                 VALUES (?1, ?2, ?3, 'q', '/tmp', ?4, ?5, ?5, ?6, 'Task', 2000)",
                rusqlite::params![id, provider, model, status, at, stop],
            )
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_refused_model_is_unavailable_until_a_later_launch_gets_past_startup() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let ts = |mins: i64| (now - Duration::minutes(mins)).to_rfc3339();
        let sol = "provider_error:model_unavailable:gpt-6.1-sol";
        insert_model(
            &store,
            "00000000-0000-4000-8000-000000000021",
            "Codex",
            "gpt-6.1-sol",
            "Failed",
            Some(sol),
            &ts(30),
        );
        insert_model(
            &store,
            "00000000-0000-4000-8000-000000000022",
            "Codex",
            "gpt-6.1-sol",
            "Failed",
            Some(sol),
            &ts(20),
        );
        // A working model of the same provider is unaffected.
        insert_model(
            &store,
            "00000000-0000-4000-8000-000000000023",
            "Codex",
            "gpt-6-luna",
            "Completed",
            None,
            &ts(10),
        );
        let episodes = store.models_unavailable(now).unwrap();
        assert_eq!(episodes.len(), 1);
        let sol_episode = &episodes[0];
        assert_eq!(
            (sol_episode.provider.as_str(), sol_episode.model.as_str()),
            ("Codex", "gpt-6.1-sol")
        );
        assert!(sol_episode.first_seen < sol_episode.last_seen);
        assert_eq!(
            sol_episode.evidence_session_id,
            "00000000-0000-4000-8000-000000000021"
        );
        assert!(
            store
                .model_unavailable("Codex", "gpt-6-luna", now)
                .unwrap()
                .is_none()
        );
        // The Codex app-server shares the Codex catalog.
        assert!(
            store
                .model_unavailable("CodexAppServer", "gpt-6.1-sol", now)
                .unwrap()
                .is_some()
        );
        // A later launch of the same model that got past startup heals it.
        insert_model(
            &store,
            "00000000-0000-4000-8000-000000000024",
            "Codex",
            "gpt-6.1-sol",
            "Completed",
            None,
            &ts(5),
        );
        assert!(store.models_unavailable(now).unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_refusal_expires_and_the_claude_unrecognized_model_reason_counts() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let ts = |mins: i64| (now - Duration::minutes(mins)).to_rfc3339();
        // Older than the probe window: the model is admitted again.
        insert_model(
            &store,
            "00000000-0000-4000-8000-000000000031",
            "Codex",
            "gpt-6-astra",
            "Failed",
            Some("provider_error:model_unavailable:gpt-6-astra"),
            &ts(60 * 24),
        );
        assert!(store.models_unavailable(now).unwrap().is_empty());
        // The bare Claude `unrecognized_model` reason names the session's model.
        insert_model(
            &store,
            "00000000-0000-4000-8000-000000000032",
            "Claude",
            "claude-haiku-5-5",
            "Failed",
            Some("provider_model_unrecognized"),
            &ts(3),
        );
        let episodes = store.models_unavailable(now).unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(
            (episodes[0].provider.as_str(), episodes[0].model.as_str()),
            ("Claude", "claude-haiku-5-5")
        );
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
