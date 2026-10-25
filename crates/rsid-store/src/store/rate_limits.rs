//! Account-level provider rate-limit window persistence (V99, P1-B).
//!
//! The Claude CLI emits a `rate_limit_event` carrying the account's plan-window
//! utilization. RSI dropped it on the stream loop's catch-all arm, which meant
//! the single highest-value signal for a multi-session manager — "how much of
//! my plan window is left before I launch ten more agents?" — was never
//! visible anywhere.
//!
//! Utilization is an ACCOUNT fact, not a session fact: every concurrent session
//! reports the same windows. It is therefore stored as a daemon-wide
//! latest-wins snapshot keyed by `(provider, window_key)` rather than as
//! session columns, and durably so a daemon or TUI restart still has the last
//! known value.

use super::Store;
use super::row_mappers::{session_provider_to_str, str_to_session_provider};
use crate::error::Result;
use rsi_common::rpc::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use rusqlite::{Transaction, TransactionBehavior, params};

impl Store {
    /// Invalidate old telemetry without deleting account history. Internal state,
    /// not an operator setting; the operator uses ClearProviderUsageHold.
    pub fn invalidate_provider_rate_limits(
        &self,
        provider: rsi_common::types::SessionProvider,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let key = format!(
            "provider_rate_limits.invalidated_at.{}",
            session_provider_to_str(provider)
        );
        let previous = self.get_daemon_setting(&key)?;
        if previous
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| at >= now)
        {
            return Ok(());
        }
        self.set_daemon_setting(
            &key,
            &now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        )
    }

    /// Compare opaque account fingerprints across probes and daemon restarts.
    /// The first identifiable account establishes a baseline; missing identity
    /// must never clear a known hold. No personal account fields are persisted.
    pub fn observe_provider_rate_limit_account(
        &self,
        provider: rsi_common::types::SessionProvider,
        fingerprint: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let key = format!(
            "provider_rate_limits.account.{}",
            session_provider_to_str(provider)
        );
        let previous = self.get_daemon_setting(&key)?;
        let changed = previous.as_deref().is_some_and(|old| old != fingerprint);
        if changed {
            self.invalidate_provider_rate_limits(provider, now)?;
        }
        if previous.as_deref() != Some(fingerprint) {
            self.set_daemon_setting(&key, fingerprint)?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Fresh evidence is required after a released wake was refused again.
    pub(super) fn provider_capacity_observed_since(
        &self,
        provider: rsi_common::types::SessionProvider,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let invalidated_at = self.get_daemon_setting(&format!(
            "provider_rate_limits.invalidated_at.{}",
            session_provider_to_str(provider),
        ))?;
        let switched = invalidated_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| at > since);
        Ok(switched
            || self
                .load_provider_rate_limit_snapshots()?
                .iter()
                .any(|snapshot| snapshot.provider == provider && snapshot.observed_at > since))
    }

    /// Keep a durable marker even for a wake outside a manager notice cohort.
    pub fn mark_provider_window_wake_hold(
        &self,
        id: uuid::Uuid,
        provider: rsi_common::types::SessionProvider,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_jobs SET schedule_json=json_set(schedule_json,'$.provider_window_hold',?2)
             WHERE id=?1 AND enabled=1 AND json_valid(schedule_json)",
            params![id.to_string(), session_provider_to_str(provider)],
        )?;
        Ok(())
    }

    /// Manager episodes release their own wakes with their normal bounded
    /// staggering. Wakes without an episode must also recover on a switch.
    pub(super) fn release_unmanaged_provider_window_wakes(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let mut statement = self.conn.prepare(
            "SELECT id, json_extract(schedule_json,'$.provider_window_hold') FROM scheduled_jobs j
             WHERE enabled=1 AND json_valid(schedule_json)
               AND json_extract(schedule_json,'$.provider_window_hold') IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_events e
                 WHERE e.kind='provider_window_work' AND json_extract(e.payload_json,'$.kind')='wake'
                   AND json_extract(e.payload_json,'$.id')=j.id)",
        )?;
        let held = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, provider) in held {
            let provider = str_to_session_provider(&provider)?;
            if self
                .provider_usage_window_hold_until(provider, now)?
                .is_none()
            {
                self.conn.execute(
                    "UPDATE scheduled_jobs SET next_fire_at=MIN(next_fire_at,?2),
                       schedule_json=json_remove(schedule_json,'$.provider_window_hold')
                     WHERE id=?1 AND enabled=1",
                    params![id, now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)],
                )?;
            }
        }
        Ok(())
    }

    /// Upsert every window in a snapshot, latest-wins.
    ///
    /// Windows are keyed by the provider's own `unifiedWindows` key rather than
    /// by an enum, so a provider that starts reporting a third window is
    /// captured rather than dropped.
    pub fn upsert_provider_rate_limit_snapshot(
        &self,
        snapshot: &ProviderRateLimitSnapshot,
        observed_session_id: Option<uuid::Uuid>,
    ) -> Result<()> {
        let provider = session_provider_to_str(snapshot.provider);
        let observed_at = snapshot
            .observed_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let observed_session = observed_session_id.map(|id| id.to_string());

        for window in &snapshot.windows {
            self.conn.execute(
                "INSERT INTO provider_rate_limit_windows (
                     provider, window_key, utilization, resets_at_epoch, status,
                     rate_limit_type, overage_status, is_using_overage,
                     observed_at, observed_session_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(provider, window_key) DO UPDATE SET
                     utilization         = excluded.utilization,
                     resets_at_epoch     = excluded.resets_at_epoch,
                     status              = excluded.status,
                     rate_limit_type     = excluded.rate_limit_type,
                     overage_status      = excluded.overage_status,
                     is_using_overage    = excluded.is_using_overage,
                     observed_at         = excluded.observed_at,
                     observed_session_id = excluded.observed_session_id",
                params![
                    provider,
                    window.window_key,
                    window.utilization,
                    window.resets_at_epoch,
                    snapshot.status.as_deref(),
                    snapshot.rate_limit_type.as_deref(),
                    snapshot.overage_status.as_deref(),
                    i64::from(snapshot.is_using_overage),
                    observed_at,
                    observed_session.as_deref(),
                ],
            )?;
        }
        Ok(())
    }

    /// Load the latest snapshot per provider, ordered by provider then window
    /// key (both ascending) so the output is stable across calls.
    ///
    /// The ordering is alphabetical, not chronological: `window_key` values are
    /// names like `five_hour` and `seven_day`, so this is a deterministic
    /// presentation order, not "soonest reset first". Callers that need the
    /// nearest-expiry window must compare `resets_at_epoch` themselves.
    ///
    /// Rows whose `provider` string does not parse are skipped rather than
    /// failing the whole read: this is advisory telemetry, and a health-status
    /// call must not error because one stale row is unreadable.
    pub fn load_provider_rate_limit_snapshots(&self) -> Result<Vec<ProviderRateLimitSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, window_key, utilization, resets_at_epoch, status,
                    rate_limit_type, overage_status, is_using_overage, observed_at
             FROM provider_rate_limit_windows
             WHERE observed_at > COALESCE((SELECT value FROM daemon_settings
                 WHERE key = 'provider_rate_limits.invalidated_at.' || provider), '')
             ORDER BY provider ASC, window_key ASC",
        )?;

        struct Row {
            provider: String,
            window_key: String,
            utilization: f64,
            resets_at_epoch: Option<i64>,
            status: Option<String>,
            rate_limit_type: Option<String>,
            overage_status: Option<String>,
            is_using_overage: i64,
            observed_at: String,
        }

        let rows = stmt
            .query_map([], |row| {
                Ok(Row {
                    provider: row.get(0)?,
                    window_key: row.get(1)?,
                    utilization: row.get(2)?,
                    resets_at_epoch: row.get(3)?,
                    status: row.get(4)?,
                    rate_limit_type: row.get(5)?,
                    overage_status: row.get(6)?,
                    is_using_overage: row.get(7)?,
                    observed_at: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut snapshots: Vec<ProviderRateLimitSnapshot> = Vec::new();
        for row in rows {
            let Ok(provider) = str_to_session_provider(&row.provider) else {
                tracing::warn!(
                    provider = %row.provider,
                    "Skipping rate-limit row with unparseable provider"
                );
                continue;
            };
            let Ok(observed_at) = chrono::DateTime::parse_from_rfc3339(&row.observed_at) else {
                tracing::warn!(
                    observed_at = %row.observed_at,
                    "Skipping rate-limit row with unparseable observed_at"
                );
                continue;
            };
            let observed_at = observed_at.with_timezone(&chrono::Utc);

            let window = ProviderRateLimitWindow {
                window_key: row.window_key,
                utilization: row.utilization,
                resets_at_epoch: row.resets_at_epoch,
            };

            match snapshots.iter_mut().find(|s| s.provider == provider) {
                Some(existing) => {
                    // Keep the newest observation's snapshot-level fields.
                    if observed_at > existing.observed_at {
                        existing.observed_at = observed_at;
                        existing.status = row.status;
                        existing.rate_limit_type = row.rate_limit_type;
                        existing.overage_status = row.overage_status;
                        existing.is_using_overage = row.is_using_overage != 0;
                    }
                    existing.windows.push(window);
                }
                None => snapshots.push(ProviderRateLimitSnapshot {
                    provider,
                    status: row.status,
                    rate_limit_type: row.rate_limit_type,
                    overage_status: row.overage_status,
                    is_using_overage: row.is_using_overage != 0,
                    observed_at,
                    windows: vec![window],
                    stale_reason: None,
                }),
            }
        }
        Ok(snapshots)
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-03")
))]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use rsi_common::types::SessionProvider;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn account_switch_invalidates_old_windows_and_fresh_usage_enforces_again() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let snapshot = |provider, observed_at| ProviderRateLimitSnapshot {
            provider,
            status: Some("rejected".into()),
            rate_limit_type: None,
            overage_status: None,
            is_using_overage: false,
            observed_at,
            windows: vec![ProviderRateLimitWindow {
                window_key: "seven_day".into(),
                utilization: 1.0,
                resets_at_epoch: Some((now + Duration::days(4)).timestamp()),
            }],
            stale_reason: None,
        };
        store
            .upsert_provider_rate_limit_snapshot(&snapshot(SessionProvider::Claude, now), None)
            .unwrap();
        store
            .upsert_provider_rate_limit_snapshot(&snapshot(SessionProvider::Codex, now), None)
            .unwrap();
        // The first observation cannot establish which account old rows belonged to.
        assert!(
            !store
                .observe_provider_rate_limit_account(SessionProvider::Claude, "account-a", now)
                .unwrap()
        );
        assert!(
            !store
                .observe_provider_rate_limit_account(SessionProvider::Claude, "account-a", now)
                .unwrap()
        );
        assert!(
            store
                .provider_usage_window_hold_until(SessionProvider::Claude, Utc::now())
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .observe_provider_rate_limit_account(
                    SessionProvider::Claude,
                    "account-b",
                    now + Duration::seconds(1)
                )
                .unwrap()
        );
        assert!(
            store
                .provider_usage_window_hold_until(SessionProvider::Claude, Utc::now())
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .provider_usage_window_hold_until(SessionProvider::Codex, Utc::now())
                .unwrap()
                .is_some()
        );
        let retained: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM provider_rate_limit_windows",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 2, "invalidation preserves the rows");
        assert!(
            !store
                .observe_provider_rate_limit_account(
                    SessionProvider::Claude,
                    "account-b",
                    now + Duration::seconds(2)
                )
                .unwrap()
        );
        store
            .upsert_provider_rate_limit_snapshot(
                &snapshot(SessionProvider::Claude, now + Duration::seconds(3)),
                None,
            )
            .unwrap();
        assert!(
            store
                .provider_usage_window_hold_until(SessionProvider::Claude, Utc::now())
                .unwrap()
                .is_some(),
            "fresh exhausted telemetry still gates launches"
        );
        store
            .invalidate_provider_rate_limits(SessionProvider::Claude, now + Duration::seconds(4))
            .unwrap();
        assert!(
            store
                .provider_usage_window_hold_until(SessionProvider::Claude, Utc::now())
                .unwrap()
                .is_none()
        );
    }
}
