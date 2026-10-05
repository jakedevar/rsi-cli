//! Active Codex usage-limit hold (#572), derived from rows the daemon already
//! writes: a session whose `stop_reason` is the typed usage-limit reason plus
//! its terminal `Process Error` conversation event, which carries the exact
//! provider text. No schema change; a restart loses nothing.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::Store;
use crate::error::Result;
use crate::provider_exhaustion::{UsageLimitHold, hold_until};
use crate::store_support::provider_defaults::CODEX_USAGE_LIMIT_STOP_REASON;

/// Failures older than this are never consulted for a hold.
const HOLD_LOOKBACK: Duration = Duration::days(8);
/// Recent exhausted sessions considered per check.
const MAX_CANDIDATES: i64 = 20;
/// Failure count window used to grow the backoff floor.
const ATTEMPT_WINDOW: Duration = Duration::hours(24);
/// Cap on the counted failures (the backoff cap is reached well before).
const MAX_COUNTED_ATTEMPTS: i64 = 16;

fn day_prefix(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

fn parse(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

impl Store {
    /// The latest hold on automated Codex dispatch that is still in force at
    /// `now`, across every Codex session on this machine (they share one
    /// provider account). `None` when no recent usage-limit failure has a
    /// retry-after or backoff floor in the future.
    pub fn codex_usage_limit_hold(&self, now: DateTime<Utc>) -> Result<Option<UsageLimitHold>> {
        let mut candidates = self.conn.prepare(
            "SELECT id FROM sessions
             WHERE stop_reason = ?1 AND provider IN ('Codex','CodexAppServer')
               AND updated_at >= ?2
             ORDER BY updated_at DESC LIMIT ?3",
        )?;
        let ids: Vec<String> = candidates
            .query_map(
                rusqlite::params![
                    CODEX_USAGE_LIMIT_STOP_REASON,
                    day_prefix(now - HOLD_LOOKBACK),
                    MAX_CANDIDATES
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<std::result::Result<_, _>>()?;
        let mut best: Option<UsageLimitHold> = None;
        for id in ids {
            let Ok(session_id) = Uuid::parse_str(&id) else {
                continue;
            };
            let Some((text, failed_at)) = self.latest_usage_limit_failure(&id)? else {
                continue;
            };
            let attempts = self.recent_usage_limit_failures(&id, failed_at)?;
            let until = hold_until(&text, failed_at, attempts);
            if until > now && best.as_ref().is_none_or(|held| until > held.until) {
                best = Some(UsageLimitHold {
                    until,
                    provider_text: text,
                    session_id,
                });
            }
        }
        Ok(best)
    }

    /// The provider text and time of the session's latest terminal
    /// usage-limit `Process Error` event.
    fn latest_usage_limit_failure(
        &self,
        session_id: &str,
    ) -> Result<Option<(String, DateTime<Utc>)>> {
        let row: Option<(String, String)> = match self.conn.query_row(
            "SELECT content, created_at FROM conversation_events
             WHERE session_id = ?1
               AND content LIKE '**Process Error%'
               AND content LIKE '%You''ve hit your usage limit%'
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ) {
            Ok(row) => Some(row),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(error) => return Err(error.into()),
        };
        Ok(row.and_then(|(content, created_at)| {
            Some((strip_event_frame(&content), parse(&created_at)?))
        }))
    }

    /// Terminal usage-limit failures of the session in the 24 h up to
    /// `failed_at`; at least 1.
    fn recent_usage_limit_failures(
        &self,
        session_id: &str,
        failed_at: DateTime<Utc>,
    ) -> Result<u32> {
        let since =
            (failed_at - ATTEMPT_WINDOW).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT 1 FROM conversation_events
                WHERE session_id = ?1
                  AND content LIKE '**Process Error%'
                  AND content LIKE '%You''ve hit your usage limit%'
                  AND created_at >= ?2
                LIMIT ?3)",
            rusqlite::params![session_id, since, MAX_COUNTED_ATTEMPTS],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(count.max(1)).unwrap_or(1))
    }
}

/// The daemon renders a provider diagnostic as ``**heading**\n```\n<text>\n```
/// ``; the exact provider text is the fenced body.
fn strip_event_frame(content: &str) -> String {
    let body = content
        .split_once("```\n")
        .map_or(content, |(_, rest)| rest);
    body.trim_end_matches("```").trim().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rsi_common::types::{ConversationEvent, EventType, Role, SessionProvider, SessionStatus};

    const TEXT: &str = "Error running remote compact task: You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 7th, 2026 1:05 PM.";

    pub(super) fn seed_exhausted_session(
        store: &Store,
        provider: SessionProvider,
        text: &str,
        failed_at: DateTime<Utc>,
        failures: u32,
    ) -> Uuid {
        let mut session = crate::store::tests::make_test_session();
        session.id = Uuid::new_v4();
        session.provider = provider;
        session.status = SessionStatus::Failed;
        session.stop_reason = Some(CODEX_USAGE_LIMIT_STOP_REASON.into());
        session.updated_at = failed_at;
        store.insert_session(&session).unwrap();
        for sequence in 1..=failures {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: session.id,
                    sequence: i32::try_from(sequence).unwrap(),
                    event_type: EventType::Message,
                    role: Some(Role::Assistant),
                    content: format!("**Process Error (codex_event)**\n```\n{text}\n```"),
                    tool_name: None,
                    tool_input: None,
                    created_at: failed_at,
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
        }
        session.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn usage_limit_failure_holds_dispatch_until_the_provider_retry_after() {
        let store = Store::open_in_memory().unwrap();
        let failed_at = Utc.with_ymd_and_hms(2026, 9, 4, 1, 36, 31).unwrap();
        let id = seed_exhausted_session(&store, SessionProvider::Codex, TEXT, failed_at, 1);

        let now = failed_at + Duration::hours(1);
        let hold = store.codex_usage_limit_hold(now).unwrap().expect("hold");
        assert_eq!(
            hold.until,
            Utc.with_ymd_and_hms(2026, 9, 7, 13, 5, 0).unwrap()
        );
        assert_eq!(hold.session_id, id);
        assert_eq!(hold.provider_text, TEXT, "exact provider text is kept");

        let after = Utc.with_ymd_and_hms(2026, 9, 7, 13, 6, 0).unwrap();
        assert_eq!(store.codex_usage_limit_hold(after).unwrap(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn usage_limit_without_a_time_holds_for_a_bounded_growing_backoff() {
        let store = Store::open_in_memory().unwrap();
        let failed_at = Utc.with_ymd_and_hms(2026, 9, 4, 1, 36, 31).unwrap();
        let text = "You've hit your usage limit. Retry later.";
        seed_exhausted_session(&store, SessionProvider::CodexAppServer, text, failed_at, 3);
        let hold = store
            .codex_usage_limit_hold(failed_at + Duration::minutes(1))
            .unwrap()
            .expect("hold");
        assert_eq!(hold.until, failed_at + Duration::minutes(20));
        assert_eq!(
            store
                .codex_usage_limit_hold(failed_at + Duration::minutes(21))
                .unwrap(),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn other_providers_and_other_stop_reasons_never_hold() {
        let store = Store::open_in_memory().unwrap();
        let failed_at = Utc.with_ymd_and_hms(2026, 9, 4, 1, 36, 31).unwrap();
        seed_exhausted_session(&store, SessionProvider::Claude, TEXT, failed_at, 1);
        let other = seed_exhausted_session(&store, SessionProvider::Codex, TEXT, failed_at, 1);
        store
            .conn
            .execute(
                "UPDATE sessions SET stop_reason = 'provider_error:unclassified' WHERE id = ?1",
                rusqlite::params![other.to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .codex_usage_limit_hold(failed_at + Duration::minutes(1))
                .unwrap(),
            None
        );
    }
}
