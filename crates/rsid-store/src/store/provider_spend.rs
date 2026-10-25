//! Spend per `(provider, model)` over the last hour and 24 hours, for the
//! Remote providers read (hub #3796). It reads the same rows and the same
//! per-invocation cost as the fleet rollup (`fleet.rs`, #3534): a
//! session-cumulative row counts its increment, never its running total.
//!
//! The statement is bounded by a row cap, so a hub with a very busy day reports
//! `truncated` instead of a wrong sum. It is not cheap (it sorts the sessions
//! it touches), so the Remote read path never runs it inside its tight SQL
//! budget: the daemon refreshes a cached copy off that path.
use super::Store;
use crate::error::Result;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

/// Most invocation rows one spend read examines.
pub const PROVIDER_SPEND_ROW_LIMIT: usize = 50_000;

const SPEND_SQL: &str = concat!(
    "WITH ids(id) AS (SELECT DISTINCT session_id FROM model_invocations
        WHERE created_at >= ?1 AND created_at <= ?2 AND session_id IS NOT NULL)
    SELECT COALESCE(i.provider,'Unknown'), COALESCE(i.model,'Unknown'), i.created_at,
    CASE WHEN i.session_id IS NULL THEN i.estimated_cost_usd ELSE ci.cost_increment_usd END
    FROM (SELECT id,provider,model,created_at,estimated_cost_usd,session_id
          FROM model_invocations WHERE created_at >= ?1 AND created_at <= ?2
          ORDER BY created_at DESC, id LIMIT ?3) i
    LEFT JOIN (",
    crate::model_control::cost::cost_increment_for_ids_sql!(),
    ") ci ON ci.invocation_id = i.id"
);

/// Spend of one model of one provider. An unpriced invocation counts zero.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderModelSpend {
    pub provider: String,
    pub model: String,
    pub cost_1h_usd: f64,
    pub cost_24h_usd: f64,
}

/// Every `(provider, model)` pair with an invocation in the last 24 hours.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSpend {
    pub models: Vec<ProviderModelSpend>,
    /// The read stopped at its row cap: the sums are lower bounds.
    pub truncated: bool,
}

fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// The spend read over a borrowed connection, with its row cap given.
///
/// # Errors
///
/// Returns the SQL error of a failed statement.
pub fn provider_spend_with_limit(
    conn: &Connection,
    now: DateTime<Utc>,
    limit: usize,
) -> Result<ProviderSpend> {
    let mut statement = conn.prepare(SPEND_SQL)?;
    let mut rows = statement.query(params![
        timestamp(now - Duration::hours(24)),
        timestamp(now),
        i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
    ])?;
    let hour_ago = now - Duration::hours(1);
    let mut sums: BTreeMap<(String, String), (f64, f64)> = BTreeMap::new();
    let mut counted = 0;
    let mut truncated = false;
    while let Some(row) = rows.next()? {
        if counted == limit {
            truncated = true;
            break;
        }
        counted += 1;
        let provider: String = row.get(0)?;
        let model: String = row.get(1)?;
        let created: String = row.get(2)?;
        let cost: Option<f64> = row.get(3)?;
        let Ok(created) = DateTime::parse_from_rfc3339(&created) else {
            continue;
        };
        let created = created.with_timezone(&Utc);
        if created > now || created < now - Duration::hours(24) {
            continue;
        }
        let cost = cost.filter(|cost| cost.is_finite()).unwrap_or(0.0);
        let entry = sums.entry((provider, model)).or_default();
        entry.1 += cost;
        if created >= hour_ago {
            entry.0 += cost;
        }
    }
    Ok(ProviderSpend {
        models: sums
            .into_iter()
            .map(|((provider, model), (hour, day))| ProviderModelSpend {
                provider,
                model,
                cost_1h_usd: hour,
                cost_24h_usd: day,
            })
            .collect(),
        truncated,
    })
}

impl Store {
    /// Spend per `(provider, model)` over the last hour and 24 hours.
    ///
    /// # Errors
    ///
    /// Returns the SQL error of a failed statement.
    pub fn provider_spend(&self, now: DateTime<Utc>) -> Result<ProviderSpend> {
        provider_spend_with_limit(&self.conn, now, PROVIDER_SPEND_ROW_LIMIT)
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-01")
))]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn invocation(
        store: &Store,
        provider: &str,
        model: &str,
        at: DateTime<Utc>,
        cost: Option<f64>,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                   (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    trigger_source,provider,model,created_at,estimated_cost_usd)
                 VALUES (?1,'model.call','cli','foreground','paid','admitted','completed','test',
                         ?2,?3,?4,?5)",
                params![
                    Uuid::new_v4().to_string(),
                    provider,
                    model,
                    timestamp(at),
                    cost
                ],
            )
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn spend_sums_by_provider_and_model_in_the_hour_and_day_windows() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        invocation(
            &store,
            "Claude",
            "opus",
            now - Duration::minutes(5),
            Some(1.5),
        );
        invocation(
            &store,
            "Claude",
            "opus",
            now - Duration::hours(3),
            Some(2.0),
        );
        invocation(
            &store,
            "Claude",
            "haiku",
            now - Duration::minutes(30),
            Some(0.25),
        );
        invocation(&store, "Codex", "gpt", now - Duration::hours(2), None);
        // Older than 24 hours: outside both windows.
        invocation(
            &store,
            "Claude",
            "opus",
            now - Duration::hours(30),
            Some(100.0),
        );
        let spend = store.provider_spend(now).unwrap();
        assert!(!spend.truncated);
        let find = |provider: &str, model: &str| {
            spend
                .models
                .iter()
                .find(|row| row.provider == provider && row.model == model)
                .map(|row| (row.cost_1h_usd, row.cost_24h_usd))
        };
        assert_eq!(find("Claude", "opus"), Some((1.5, 3.5)));
        assert_eq!(find("Claude", "haiku"), Some((0.25, 0.25)));
        // Unpriced counts zero but the pair is still reported.
        assert_eq!(find("Codex", "gpt"), Some((0.0, 0.0)));
        assert_eq!(spend.models.len(), 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn spend_reports_truncation_at_its_row_cap() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        for minute in 0..5 {
            invocation(
                &store,
                "Claude",
                "opus",
                now - Duration::minutes(minute + 1),
                Some(1.0),
            );
        }
        let spend = provider_spend_with_limit(&store.conn, now, 3).unwrap();
        assert!(spend.truncated);
        assert_eq!(spend.models[0].cost_24h_usd, 3.0);
        let exact = provider_spend_with_limit(&store.conn, now, 5).unwrap();
        assert!(!exact.truncated);
        assert_eq!(exact.models[0].cost_24h_usd, 5.0);
    }
}
