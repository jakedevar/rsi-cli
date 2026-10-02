//! Read-only efficiency metrics for issue #1018.

use crate::error::Result;
use crate::store::Store;
use crate::store::efficiency_publications::{Publication, percentile, read_publications};
use chrono::{DateTime, Utc};
use rsi_common::rpc::{
    EfficiencyMetricTargets, EfficiencyMetricValues, EfficiencyMetricsGroupBy,
    EfficiencyMetricsResponse, EfficiencyMetricsRow, GetEfficiencyMetricsParams,
};
use rusqlite::params;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Debug)]
struct SessionNode {
    parent_id: Option<String>,
    continued_from: Option<String>,
    lead_session_id: Option<String>,
    session_kind: String,
    title: Option<String>,
}

/// Per-bucket accumulators that do not travel in the response.
#[derive(Default)]
struct BucketExtras {
    gate_seconds: f64,
    latencies: Vec<f64>,
}

type BucketKey = (String, Option<Uuid>);

impl Store {
    pub fn efficiency_metrics(
        &self,
        params: GetEfficiencyMetricsParams,
        project_repo: Option<&Path>,
    ) -> Result<EfficiencyMetricsResponse> {
        if params.to <= params.from {
            return Err(crate::error::DaemonError::InvalidParam(
                "Invalid params: to must be after from".to_string(),
            ));
        }
        let from = params
            .from
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let to = params
            .to
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let sessions = self.load_session_graph()?;
        let messages = self.load_user_messages(&from, &to)?;
        let mut values: BTreeMap<(String, Option<Uuid>), EfficiencyMetricValues> = BTreeMap::new();

        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, created_at, input_tokens, output_tokens,
                    cache_read_tokens, cache_creation_tokens, estimated_cost_usd
             FROM model_invocations
             WHERE created_at >= ?1 AND created_at < ?2
               AND provider = 'Claude' AND model LIKE 'claude-opus%'
               AND foreground = 'foreground'",
        )?;
        let mut invocations = stmt
            .query_map(params![from, to], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?.unwrap_or_default().max(0),
                    row.get::<_, Option<i64>>(4)?.unwrap_or_default().max(0),
                    row.get::<_, Option<i64>>(5)?.unwrap_or_default().max(0),
                    row.get::<_, Option<i64>>(6)?.unwrap_or_default().max(0),
                    row.get::<_, Option<f64>>(7)?.unwrap_or_default().max(0.0),
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        invocations.sort_by(|left, right| left.2.cmp(&right.2));

        let mut messages_by_session: HashMap<String, Vec<(String, DateTime<Utc>, String)>> =
            HashMap::new();
        for (session_id, created_at, message_created_at, content) in messages {
            messages_by_session.entry(session_id).or_default().push((
                created_at,
                message_created_at,
                content,
            ));
        }
        for rows in messages_by_session.values_mut() {
            rows.sort_by_key(|(_, created_at, _)| *created_at);
        }

        for (created_at, session_id) in invocations.iter().map(|row| (&row.2, &row.1)) {
            if let Some(session_id) = session_id {
                if let Some(message) = messages_by_session.get(session_id).and_then(|rows| {
                    rows.iter().rev().find(|(_, message_created_at, _)| {
                        message_created_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                            <= *created_at
                    })
                }) {
                    let epic_id = epic_for_session(&sessions, session_id);
                    let day = day_key(created_at);
                    let entry = values.entry((day, epic_id)).or_default();
                    if message
                        .2
                        .starts_with("<rsid-daemon-message source=\"scheduled-wake\">")
                    {
                        entry.scheduled_wake_user_messages += 1;
                    } else if message
                        .2
                        .starts_with("<rsid-daemon-message source=\"terminal-watch\">")
                    {
                        entry.terminal_watch_user_messages += 1;
                    } else if message
                        .2
                        .starts_with("<rsid-daemon-message source=\"manager\">")
                    {
                        entry.manager_notice_user_messages += 1;
                    }
                }
            }
        }

        let lead_ids: HashSet<String> = sessions
            .values()
            .filter_map(|session| session.lead_session_id.clone())
            .collect();
        for invocation in &invocations {
            let Some(session_id) = invocation.1.as_ref() else {
                continue;
            };
            if !is_lead_session(&sessions, session_id, &lead_ids) {
                continue;
            }
            let epic_id = epic_for_session(&sessions, session_id);
            let day = day_key(&invocation.2);
            let entry = values.entry((day, epic_id)).or_default();
            entry.lead_turns += 1;
            let is_poll = messages_by_session
                .get(session_id)
                .and_then(|rows| {
                    rows.iter().rev().find(|(_, message_created_at, _)| {
                        message_created_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                            <= invocation.2
                    })
                })
                .is_some_and(|(_, _, content)| {
                    content.starts_with("<rsid-daemon-message source=\"scheduled-wake\">")
                });
            if is_poll {
                entry.poll_turns += 1;
            }
        }

        for invocation in &invocations {
            let epic_id = invocation
                .1
                .as_deref()
                .and_then(|session_id| epic_for_session(&sessions, session_id));
            let day = day_key(&invocation.2);
            let entry = values.entry((day, epic_id)).or_default();
            entry.opus_input_tokens += (invocation.3 + invocation.5 + invocation.6)
                .try_into()
                .unwrap_or(u64::MAX);
            entry.opus_output_tokens += invocation.4.try_into().unwrap_or(u64::MAX);
            entry.opus_estimated_cost_usd += invocation.7;
        }

        let mut stmt = self.conn.prepare(
            "SELECT epic_id, state, terminal_at
             FROM manager_review_assignments
             WHERE terminal_at >= ?1 AND terminal_at < ?2",
        )?;
        let reviews = stmt
            .query_map(params![from, to], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (epic_id, state, terminal_at) in reviews {
            let epic_id = Uuid::parse_str(&epic_id).ok();
            let entry = values.entry((day_key(&terminal_at), epic_id)).or_default();
            match state.as_str() {
                "submitted" => entry.reviews_submitted += 1,
                "failed" => entry.reviews_failed += 1,
                "superseded" => entry.reviews_superseded += 1,
                _ => {}
            }
        }

        let jobs = self.load_landing_jobs(&from, &to)?;
        let publications =
            project_repo.and_then(|repo| read_publications(repo, params.from, params.to));
        let mut extras: BTreeMap<BucketKey, BucketExtras> = BTreeMap::new();
        for (owner, created_at, finished_at, _) in &jobs {
            let epic_id = epic_for_session(&sessions, owner);
            let key = (day_key(finished_at), epic_id);
            values.entry(key.clone()).or_default().landing_jobs += 1;
            extras.entry(key).or_default().gate_seconds += seconds_between(created_at, finished_at);
        }
        if let Some(publications) = &publications {
            for publication in publications {
                let observed = publication
                    .observed_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
                let epic_id = publication_epic(&sessions, &jobs, publication);
                let key = (day_key(&observed), epic_id);
                let entry = values.entry(key.clone()).or_default();
                entry.landings = Some(entry.landings.unwrap_or(0) + publication.steps.len() as u64);
                extras
                    .entry(key)
                    .or_default()
                    .latencies
                    .extend(publication.steps.iter().map(|step| {
                        (publication.observed_at - step.committer_time)
                            .num_seconds()
                            .max(0) as f64
                    }));
            }
            for value in values.values_mut() {
                value.landings.get_or_insert(0);
            }
        }

        let mut day_totals: BTreeMap<String, EfficiencyMetricValues> = BTreeMap::new();
        let mut day_extras: BTreeMap<String, BucketExtras> = BTreeMap::new();
        for ((day, _), values) in &values {
            let totals = day_totals.entry(day.clone()).or_default();
            totals.opus_input_tokens += values.opus_input_tokens;
            totals.opus_output_tokens += values.opus_output_tokens;
            totals.opus_estimated_cost_usd += values.opus_estimated_cost_usd;
            totals.landings = match (totals.landings, values.landings) {
                (Some(left), Some(right)) => Some(left + right),
                (left, right) => left.or(right),
            };
            totals.landing_jobs += values.landing_jobs;
            totals.lead_turns += values.lead_turns;
            totals.poll_turns += values.poll_turns;
            totals.scheduled_wake_user_messages += values.scheduled_wake_user_messages;
            totals.terminal_watch_user_messages += values.terminal_watch_user_messages;
            totals.manager_notice_user_messages += values.manager_notice_user_messages;
            totals.reviews_submitted += values.reviews_submitted;
            totals.reviews_failed += values.reviews_failed;
            totals.reviews_superseded += values.reviews_superseded;
        }
        for ((day, _), bucket) in &extras {
            let total = day_extras.entry(day.clone()).or_default();
            total.gate_seconds += bucket.gate_seconds;
            total.latencies.extend_from_slice(&bucket.latencies);
        }
        let empty = BucketExtras::default();
        for (key, values) in values.iter_mut() {
            finish_landing_values(values, extras.get(key).unwrap_or(&empty));
        }
        for (day, values) in day_totals.iter_mut() {
            finish_landing_values(values, day_extras.get(day).unwrap_or(&empty));
        }

        let mut rows = if params.group_by == EfficiencyMetricsGroupBy::Day {
            day_totals
                .into_iter()
                .map(|(day, values)| (day, None, values))
                .collect::<Vec<_>>()
        } else {
            values
                .into_iter()
                .map(|((day, epic_id), values)| (day, epic_id, values))
                .chain(
                    day_totals
                        .into_iter()
                        .map(|(day, values)| (day, None, values)),
                )
                .collect::<Vec<_>>()
        };
        for (_, _, values) in &mut rows {
            values.poll_turn_share = if values.lead_turns == 0 {
                0.0
            } else {
                values.poll_turns as f64 / values.lead_turns as f64
            };
            let settled = values.reviews_submitted + values.reviews_failed;
            values.reviewer_receipt_success_rate = if settled == 0 {
                None
            } else {
                Some(values.reviews_submitted as f64 / settled as f64)
            };
        }

        let rows = rows
            .into_iter()
            .map(|(day, epic_id, values)| {
                let epic_title = epic_id
                    .as_ref()
                    .map(|id| id.to_string())
                    .and_then(|id| sessions.get(&id))
                    .and_then(|session| session.title.clone());
                EfficiencyMetricsRow {
                    day,
                    epic_id,
                    epic_title,
                    values,
                }
            })
            .collect();
        Ok(EfficiencyMetricsResponse {
            from: params.from,
            to: params.to,
            group_by: params.group_by,
            targets: EfficiencyMetricTargets::default(),
            rows,
        })
    }

    /// Landing jobs (`agent_jobs` kind `landing`) that finished in `[from, to)`:
    /// owner, created, finished and the published tip when the job landed.
    fn load_landing_jobs(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<(String, String, String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT owner_session_id, created_at, finished_at,
                    json_extract(result_json, '$.landed_sha')
             FROM agent_jobs
             WHERE kind = 'landing' AND finished_at >= ?1 AND finished_at < ?2",
        )?;
        stmt.query_map(params![from, to], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
    }

    fn load_session_graph(&self) -> Result<HashMap<String, SessionNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, parent_id, continued_from, lead_session_id, session_kind, title
             FROM sessions",
        )?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SessionNode {
                    parent_id: row.get(1)?,
                    continued_from: row.get(2)?,
                    lead_session_id: row.get(3)?,
                    session_kind: row.get(4)?,
                    title: row.get(5)?,
                },
            ))
        })?
        .collect::<std::result::Result<HashMap<_, _>, _>>()
        .map_err(Into::into)
    }

    fn load_user_messages(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<(String, String, DateTime<Utc>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, created_at, content
             FROM conversation_events
             WHERE role = 'User' AND event_type = 'Message'
               AND created_at >= ?1 AND created_at < ?2",
        )?;
        stmt.query_map(params![from, to], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(1)?)
                    .map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            1,
                            "created_at".to_string(),
                            rusqlite::types::Type::Text,
                        )
                    })?
                    .with_timezone(&Utc),
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
    }
}

fn epic_for_session(sessions: &HashMap<String, SessionNode>, start: &str) -> Option<Uuid> {
    let mut current = start.to_string();
    for _ in 0..64 {
        let Some(node) = sessions.get(&current) else {
            return None;
        };
        if node.session_kind == "Epic" {
            return Uuid::parse_str(&current).ok();
        }
        match node.parent_id.clone() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
    None
}

fn is_lead_session(
    sessions: &HashMap<String, SessionNode>,
    start: &str,
    lead_ids: &HashSet<String>,
) -> bool {
    if lead_ids.contains(start) {
        return true;
    }
    let mut current = start.to_string();
    for _ in 0..64 {
        let Some(node) = sessions.get(&current) else {
            return false;
        };
        match node.continued_from.clone() {
            Some(previous) if previous != current => current = previous,
            _ => return false,
        }
        if lead_ids.contains(&current) {
            return true;
        }
    }
    false
}

/// A publication belongs to the Epic of the landing job that published one of
/// its steps; otherwise it is unattributed.
fn publication_epic(
    sessions: &HashMap<String, SessionNode>,
    jobs: &[(String, String, String, Option<String>)],
    publication: &Publication,
) -> Option<Uuid> {
    let owner = jobs.iter().find_map(|(owner, _, _, landed)| {
        let landed = landed.as_deref()?;
        publication
            .steps
            .iter()
            .any(|step| step.sha == landed)
            .then_some(owner)
    })?;
    epic_for_session(sessions, owner)
}

fn seconds_between(start: &str, end: &str) -> f64 {
    let parse = |value: &str| DateTime::parse_from_rfc3339(value).ok();
    match (parse(start), parse(end)) {
        (Some(start), Some(end)) => (end - start).num_milliseconds().max(0) as f64 / 1000.0,
        _ => 0.0,
    }
}

fn finish_landing_values(values: &mut EfficiencyMetricValues, extras: &BucketExtras) {
    values.opus_tokens_per_landing = match values.landings {
        Some(landings) if landings > 0 => Some(values.opus_input_tokens as f64 / landings as f64),
        _ => None,
    };
    values.gate_hours_per_landing = match values.landings {
        Some(landings) if landings > 0 && values.landing_jobs > 0 => {
            Some(extras.gate_seconds / 3600.0 / landings as f64)
        }
        _ => None,
    };
    values.seal_to_rolling_p50_seconds = percentile(&extras.latencies, 0.5);
    values.seal_to_rolling_p90_seconds = percentile(&extras.latencies, 0.9);
}

fn day_key(timestamp: &str) -> String {
    timestamp.chars().take(10).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    const DAY: &str = "2026-09-28";

    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("metrics.db")).unwrap();
        (dir, store)
    }

    fn insert_session(store: &Store, id: &str, parent: Option<&str>, continued_from: Option<&str>) {
        store
            .conn
            .execute(
                "INSERT INTO sessions(id, provider, query, working_dir, status, session_kind,
                                      created_at, updated_at, continued_from, parent_id)
                 VALUES(?1, 'Claude', 'test', '/tmp/rsi-metrics', 'Completed', 'Standard',
                        ?2, ?2, ?3, ?4)",
                params![id, format!("{DAY}T08:00:00Z"), continued_from, parent],
            )
            .unwrap();
    }

    fn fixture() -> (tempfile::TempDir, Store) {
        let (dir, store) = test_store();
        store
            .conn
            .execute(
                "INSERT INTO sessions(id, provider, query, working_dir, status, session_kind,
                                      created_at, updated_at, lead_session_id)
                 VALUES('00000000-0000-0000-0000-000000000001', 'Claude', '00000000-0000-0000-0000-000000000001', '/tmp/rsi-metrics', 'Completed', 'Epic',
                        ?1, ?1, '00000000-0000-0000-0000-000000000002')",
                params![format!("{DAY}T08:00:00Z")],
            )
            .unwrap();
        insert_session(
            &store,
            "00000000-0000-0000-0000-000000000002",
            Some("00000000-0000-0000-0000-000000000001"),
            None,
        );
        insert_session(
            &store,
            "00000000-0000-0000-0000-000000000003",
            Some("00000000-0000-0000-0000-000000000001"),
            Some("00000000-0000-0000-0000-000000000002"),
        );
        insert_session(
            &store,
            "00000000-0000-0000-0000-000000000004",
            Some("00000000-0000-0000-0000-000000000001"),
            None,
        );
        insert_session(&store, "00000000-0000-0000-0000-000000000008", None, None);
        for (session, hour, source, invocation_id) in [
            (
                "00000000-0000-0000-0000-000000000002",
                10,
                "scheduled-wake",
                "lead-invocation",
            ),
            (
                "00000000-0000-0000-0000-000000000002",
                11,
                "terminal-watch",
                "terminal-invocation",
            ),
            (
                "00000000-0000-0000-0000-000000000003",
                12,
                "manager",
                "successor-invocation",
            ),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO conversation_events(session_id, sequence, event_type, role,
                                                    content, created_at)
                     VALUES(?1, 1, 'Message', 'User', ?2, ?3)",
                    params![
                        session,
                        format!("<rsid-daemon-message source=\"{source}\">wake"),
                        format!("{DAY}T{hour:02}:00:00.000000000Z")
                    ],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                                   paid_risk, admission_status, status,
                                                   provider, model, trigger_source, session_id,
                                                   input_tokens, output_tokens,
                                                   cache_read_tokens, cache_creation_tokens,
                                                   estimated_cost_usd, created_at)
                     VALUES(?1, 'session.turn', 'session_lifecycle', 'foreground', 'paid_capable',
                            'admitted', 'completed', 'Claude', 'claude-opus-test', 'test', ?2,
                            100, 20, 30, 40, 1.5, ?3)",
                    params![invocation_id, session, format!("{DAY}T{hour:02}:01:00Z")],
                )
                .unwrap();
        }
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                               paid_risk, admission_status, status, provider,
                                               model, trigger_source, session_id, input_tokens,
                                               output_tokens, cache_read_tokens,
                                               cache_creation_tokens, estimated_cost_usd,
                                               created_at)
                 VALUES('excluded', 'session.turn', 'session_lifecycle', 'foreground',
                        'paid_capable', 'admitted', 'completed', 'Claude', 'claude-opus-test',
                        'test', '00000000-0000-0000-0000-000000000002', 999, 0, 0, 0, 9.0, ?1)",
                params![format!("{DAY}T13:00:00Z")],
            )
            .unwrap();
        for (assignment, state, hour) in [
            ("00000000-0000-0000-0000-000000000011", "submitted", 9),
            ("00000000-0000-0000-0000-000000000012", "submitted", 10),
            ("00000000-0000-0000-0000-000000000013", "failed", 11),
            ("00000000-0000-0000-0000-000000000014", "superseded", 12),
        ] {
            store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO manager_review_assignments(
                         assignment_id, project_id, epic_id, manager_session_id, scope_version,
                         work_key, spec_revision, author_session_id, source_sha, state,
                         row_version, request_json, request_fingerprint, created_at,
                         updated_at, terminal_at, action_operation_id, reviewer_session_id,
                         reviewer_invocation_id, reviewer_custody_id,
                         reviewer_custody_generation, failure_code, superseded_by_assignment_id)
                     VALUES(?1, '00000000-0000-0000-0000-000000000007', '00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000005', 1, ?1, 1, '00000000-0000-0000-0000-000000000006', '00000000000000000000000000000000000000' || substr(?1, -2), ?2, 1,
                            '{}', 'sha256:0000000000000000000000000000000000000000000000000000000000000000', ?3, ?3, ?3,
                            CASE WHEN ?2 = 'submitted' THEN '00000000-0000-0000-0000-00000000002' || substr(?1, -1) ELSE NULL END,
                            CASE WHEN ?2 = 'submitted' THEN '00000000-0000-0000-0000-000000000008' ELSE NULL END,
                            CASE WHEN ?2 = 'submitted' THEN '00000000-0000-0000-0000-000000000022' ELSE NULL END,
                            CASE WHEN ?2 = 'submitted' THEN '00000000-0000-0000-0000-000000000023' ELSE NULL END,
                            CASE WHEN ?2 = 'submitted' THEN 1 ELSE NULL END,
                            CASE WHEN ?2 = 'failed' THEN 'manager_review_custody_unavailable' ELSE NULL END,
                            CASE WHEN ?2 = 'superseded' THEN '00000000-0000-0000-0000-000000000012' ELSE NULL END)",
                    params![
                        assignment,
                        state,
                        format!("{DAY}T{hour:02}:00:00.000000000Z")
                    ],
                )
                .unwrap();
            store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        }
        (dir, store)
    }

    fn window() -> GetEfficiencyMetricsParams {
        GetEfficiencyMetricsParams {
            from: DateTime::parse_from_rfc3339(&format!("{DAY}T08:00:00Z"))
                .unwrap()
                .with_timezone(&Utc),
            to: DateTime::parse_from_rfc3339("2026-09-28T13:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            group_by: EfficiencyMetricsGroupBy::DayEpic,
        }
    }

    fn epic_row(response: &EfficiencyMetricsResponse) -> &EfficiencyMetricValues {
        &response
            .rows
            .iter()
            .find(|row| row.epic_id.is_some())
            .unwrap()
            .values
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn computes_exact_values_and_per_epic_totals() {
        let (_dir, store) = fixture();
        let response = store.efficiency_metrics(window(), None).unwrap();
        let epic = epic_row(&response);
        assert_eq!(epic.opus_input_tokens, 510);
        assert_eq!(epic.opus_output_tokens, 60);
        assert_eq!(epic.opus_estimated_cost_usd, 4.5);
        assert_eq!(epic.lead_turns, 3);
        assert_eq!(epic.poll_turns, 1);
        assert_eq!(epic.poll_turn_share, 1.0 / 3.0);
        assert_eq!(epic.scheduled_wake_user_messages, 1);
        assert_eq!(epic.terminal_watch_user_messages, 1);
        assert_eq!(epic.manager_notice_user_messages, 1);
        assert_eq!(epic.reviews_submitted, 2);
        assert_eq!(epic.reviews_failed, 1);
        assert_eq!(epic.reviews_superseded, 1);
        assert_eq!(epic.reviewer_receipt_success_rate, Some(2.0 / 3.0));
        assert_eq!(epic.gate_hours_per_landing, None);
        let day = response
            .rows
            .iter()
            .find(|row| row.epic_id.is_none())
            .unwrap();
        assert_eq!(day.values.opus_input_tokens, epic.opus_input_tokens);
        assert_eq!(day.values.lead_turns, epic.lead_turns);
        assert_eq!(day.values.poll_turns, epic.poll_turns);
        assert_eq!(day.values.reviews_submitted, epic.reviews_submitted);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn empty_window_returns_zero_rows_and_exclusive_to_excludes_data() {
        let (_dir, store) = fixture();
        let empty = store
            .efficiency_metrics(
                GetEfficiencyMetricsParams {
                    from: DateTime::parse_from_rfc3339("2026-09-27T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                    to: DateTime::parse_from_rfc3339("2026-09-28T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                    group_by: EfficiencyMetricsGroupBy::Day,
                },
                None,
            )
            .unwrap();
        assert_eq!(empty.rows.len(), 0);
        let response = store.efficiency_metrics(window(), None).unwrap();
        assert_eq!(epic_row(&response).opus_input_tokens, 510);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn day_only_rows_collapse_epic_attribution() {
        let (_dir, store) = fixture();
        let mut params = window();
        params.group_by = EfficiencyMetricsGroupBy::Day;
        let response = store.efficiency_metrics(params, None).unwrap();
        assert_eq!(response.rows.len(), 1);
        assert!(response.rows[0].epic_id.is_none());
        assert_eq!(response.rows[0].values.opus_input_tokens, 510);
    }

    const EPIC: &str = "00000000-0000-0000-0000-000000000001";
    const LEAD: &str = "00000000-0000-0000-0000-000000000002";

    /// Three commits authored in 2020 and published on DAY at 10:00Z as one
    /// fast-forward; returns the published tip.
    fn publish_fast_forward(repo: &Path) -> String {
        use crate::store::efficiency_publications::tests::{commit, git};
        const REF: &str = "refs/remotes/origin/rolling";
        git(repo, "1577836800 +0000", &["init", "-q", "-b", "rolling"]);
        let base = commit(repo, "1577836800 +0000", "base");
        git(
            repo,
            "1790586000 +0000",
            &["update-ref", "--create-reflog", REF, &base],
        );
        commit(repo, "1577836900 +0000", "one");
        commit(repo, "1577837000 +0000", "two");
        let tip = commit(repo, "1577837100 +0000", "three");
        git(repo, "1790589600 +0000", &["update-ref", REF, &tip]);
        tip
    }

    fn insert_landing_job(store: &Store, owner: &str, landed: Option<&str>, finished: &str) {
        let result = landed.map(|sha| format!("{{\"landed_sha\":\"{sha}\"}}"));
        store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id, owner_session_id, kind, params_json, cwd, unit_name,
                                        log_path, status_path, state, exit_code, result_json,
                                        created_at, finished_at, row_version)
                 VALUES('00000000-0000-0000-0000-0000000000a1', ?1, 'landing', '{}', '/tmp',
                        'rsi-job-a1', '/tmp/a1.log', '/tmp/a1.status', 'succeeded', 0, ?2,
                        ?3, ?4, 1)",
                params![
                    owner,
                    result,
                    format!("{DAY}T09:30:00.000000000Z"),
                    finished
                ],
            )
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn fast_forward_publication_counts_landings_gate_hours_and_latency() {
        let (_dir, store) = fixture();
        let repo = tempfile::tempdir().unwrap();
        let tip = publish_fast_forward(repo.path());
        insert_landing_job(
            &store,
            LEAD,
            Some(&tip),
            &format!("{DAY}T10:05:00.000000000Z"),
        );
        let response = store
            .efficiency_metrics(window(), Some(repo.path()))
            .unwrap();
        let epic = response
            .rows
            .iter()
            .find(|row| row.epic_id == Uuid::parse_str(EPIC).ok())
            .unwrap();
        assert_eq!(epic.values.landings, Some(3));
        assert_eq!(epic.values.landing_jobs, 1);
        assert_eq!(epic.values.opus_tokens_per_landing, Some(170.0));
        // The job ran 09:30 to 10:05: 35 minutes over three landings.
        assert_eq!(
            epic.values.gate_hours_per_landing,
            Some(2100.0 / 3600.0 / 3.0)
        );
        assert_eq!(
            epic.values.seal_to_rolling_p50_seconds,
            Some((1790589600 - 1577837000) as f64)
        );
        assert_eq!(
            epic.values.seal_to_rolling_p90_seconds,
            Some((1790589600 - 1577836900) as f64)
        );
        let day = response
            .rows
            .iter()
            .find(|row| row.epic_id.is_none() && row.values.landings.is_some_and(|n| n > 0))
            .unwrap();
        assert_eq!(day.values.landings, epic.values.landings);
        assert_eq!(
            day.values.gate_hours_per_landing,
            epic.values.gate_hours_per_landing
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn unjobbed_publication_is_unattributed_and_gate_hours_stay_unknown() {
        let (_dir, store) = fixture();
        let repo = tempfile::tempdir().unwrap();
        publish_fast_forward(repo.path());
        let mut params = window();
        params.group_by = EfficiencyMetricsGroupBy::Day;
        let response = store.efficiency_metrics(params, Some(repo.path())).unwrap();
        let day = &response.rows[0].values;
        assert_eq!(day.landings, Some(3));
        assert_eq!(day.landing_jobs, 0);
        assert_eq!(day.gate_hours_per_landing, None);
        // Without a repository the count is unknown, not zero.
        let unknown = store.efficiency_metrics(window(), None).unwrap();
        assert!(unknown.rows.iter().all(|row| row.values.landings.is_none()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn publication_only_window_reports_a_day_row_and_bounds_by_window() {
        let (_dir, store) = fixture();
        let repo = tempfile::tempdir().unwrap();
        publish_fast_forward(repo.path());
        let mut params = window();
        params.group_by = EfficiencyMetricsGroupBy::Day;
        // The publication (10:00Z) is outside [11:00Z, 13:00Z).
        params.from = DateTime::parse_from_rfc3339("2026-09-28T11:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let response = store.efficiency_metrics(params, Some(repo.path())).unwrap();
        assert_eq!(response.rows[0].values.landings, Some(0));
        assert_eq!(response.rows[0].values.opus_tokens_per_landing, None);
    }
}
