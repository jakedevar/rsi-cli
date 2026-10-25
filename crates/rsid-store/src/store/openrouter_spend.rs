//! Spend reads for the OpenRouter ceiling (#3154). The account usage the
//! provider reports is the truth; these reads cover the gap between probes (the
//! invocations settled since the last probe) and a benchmark run's own spend.
//!
//! Costs come from `model_invocations`: a provider-reported OpenRouter
//! `usage.cost` verbatim, else an estimate from the discovered or documented
//! price (`model_control::cost`). Session-cumulative purposes store running
//! totals, so every sum here goes through the increment SQL, never a raw SUM.

use super::Store;
use crate::error::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::params;
use uuid::Uuid;

impl Store {
    /// The cost of OpenRouter-billable invocations settled at or after `since`
    /// (their own cost, never a session running total). Billable providers are
    /// the `OpenRouter` provider and `Harness` (its OpenRouter-endpoint
    /// sessions record `Harness`); counting every Harness invocation
    /// over-counts a direct OpenAI or Anthropic Harness session for the short
    /// window since the last probe, the safe direction for a spend ceiling.
    /// Rows with no known cost count as nothing: the next probe corrects the
    /// account figure.
    pub fn openrouter_billable_spend_settled_since(&self, since: DateTime<Utc>) -> Result<f64> {
        self.provider_spend_settled_since(&["OpenRouter", "Harness"], since)
    }

    /// The cost of invocations by the given provider names settled at or
    /// after `since`, by the same increment rule as
    /// [`Self::openrouter_billable_spend_settled_since`] (#3417: the charge
    /// of a provider spend grant). A name is a `model_invocations.provider`
    /// value.
    pub fn provider_spend_settled_since(
        &self,
        providers: &[&str],
        since: DateTime<Utc>,
    ) -> Result<f64> {
        self.provider_spend_settled_since_for_project(providers, since, None)
    }

    /// [`Self::provider_spend_settled_since`] limited to one project's usage
    /// (#3417 F1): an invocation recorded with the project, one of the
    /// project's sessions, or one of its bench-run task sessions. `None`
    /// counts every project.
    pub fn provider_spend_settled_since_for_project(
        &self,
        providers: &[&str],
        since: DateTime<Utc>,
        project: Option<Uuid>,
    ) -> Result<f64> {
        let project = project.map(|id| id.to_string());
        let since = since.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let providers = serde_json::to_string(providers)?;
        let sessions: f64 = self.conn.query_row(
            concat!(
                "WITH ids(id) AS (
                    SELECT DISTINCT session_id FROM model_invocations
                    WHERE session_id IS NOT NULL AND admission_status = 'admitted'
                      AND provider IN (SELECT value FROM json_each(?2)) AND completed_at >= ?1
                      AND (?3 IS NULL OR project_id = ?3
                           OR session_id IN (SELECT id FROM sessions WHERE project_id = ?3)
                           OR session_id IN (SELECT bt.session_id FROM bench_tasks bt
                                             JOIN bench_runs br ON br.id = bt.run_id
                                             WHERE br.project_id = ?3))
                 )
                 SELECT COALESCE(SUM(ci.cost_increment_usd), 0) FROM (",
                crate::model_control::cost::cost_increment_for_ids_sql!(),
                ") ci JOIN model_invocations mi ON mi.id = ci.invocation_id
                 WHERE mi.admission_status = 'admitted' AND mi.completed_at >= ?1
                   AND mi.provider IN (SELECT value FROM json_each(?2))
                   AND ci.cost_increment_usd IS NOT NULL AND ci.cost_increment_usd >= 0"
            ),
            params![since, providers, project],
            |row| row.get(0),
        )?;
        let sessionless: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(estimated_cost_usd), 0) FROM model_invocations
             WHERE session_id IS NULL AND admission_status = 'admitted'
               AND provider IN (SELECT value FROM json_each(?2)) AND completed_at >= ?1
               AND (?3 IS NULL OR project_id = ?3)
               AND estimated_cost_usd IS NOT NULL AND estimated_cost_usd >= 0",
            params![since, providers, project],
            |row| row.get(0),
        )?;
        Ok(sessions + sessionless)
    }

    /// The spend of the given sessions: the sum of each invocation's own cost.
    /// A benchmark run reads its tasks' sessions here against its
    /// `run_budget_usd`.
    pub fn sessions_invocation_spend(&self, sessions: &[Uuid]) -> Result<f64> {
        if sessions.is_empty() {
            return Ok(0.0);
        }
        let ids = serde_json::to_string(&sessions.iter().map(Uuid::to_string).collect::<Vec<_>>())?;
        Ok(self.conn.query_row(
            concat!(
                "WITH ids(id) AS (SELECT value FROM json_each(?1))
                 SELECT COALESCE(SUM(ci.cost_increment_usd), 0) FROM (",
                crate::model_control::cost::cost_increment_for_ids_sql!(),
                ") ci
                 WHERE ci.admission_status = 'admitted'
                   AND ci.cost_increment_usd IS NOT NULL AND ci.cost_increment_usd >= 0"
            ),
            params![ids],
            |row| row.get(0),
        )?)
    }
}
