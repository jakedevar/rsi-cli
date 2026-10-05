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
use rusqlite::params;

impl Store {
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
                }),
            }
        }
        Ok(snapshots)
    }
}
