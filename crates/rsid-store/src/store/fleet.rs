//! Bounded indexed reads; rates attribute usage to invocation creation time.
use super::{
    Store,
    row_mappers::{SESSION_COLUMNS, map_session_row},
};
use crate::error::Result;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::fleet::*;
use rusqlite::params;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const AGENT_LIMIT: usize = 2048;
const USAGE_LIMIT: usize = 50000;
const ACTIVE_SQL: &str = "SELECT id FROM sessions WHERE status IN ('Starting','Running','WaitingApproval')
    AND updated_at <= ?1 AND COALESCE(session_kind,'Standard') NOT IN ('Group','Epic') ORDER BY updated_at DESC, id LIMIT ?2";
const USAGE_SQL: &str = "SELECT COALESCE(i.project_id,''), COALESCE(p.name,'Unassigned'),
    COALESCE(i.provider,'Unknown'), COALESCE(i.model,'Unknown'), i.created_at, i.status,
    i.input_tokens, i.output_tokens, i.cache_read_tokens, i.cache_creation_tokens, i.estimated_cost_usd, i.session_id
    FROM (SELECT project_id,provider,model,created_at,status,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,estimated_cost_usd,session_id FROM model_invocations WHERE created_at >= ?1 AND created_at <= ?2
          ORDER BY created_at DESC, id LIMIT ?3) i LEFT JOIN projects p ON p.id=i.project_id";
fn timestamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}
fn group<'a>(
    groups: &'a mut BTreeMap<(String, String), FleetGroup>,
    dim: &str,
    key: &str,
    label: &str,
) -> &'a mut FleetGroup {
    groups
        .entry((dim.into(), key.into()))
        .or_insert_with(|| FleetGroup {
            dimension: dim.into(),
            key: key.into(),
            label: label.into(),
            ..Default::default()
        })
}
/// #1240: which rows of the one fleet aggregation a manager node's rollup
/// keeps. The bounded queries are the same for every scope; the scope only
/// filters their rows, so a node's counts are #1232's totals filtered to its
/// coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetScope {
    /// Every row (#1232's operator fleet view).
    All,
    /// Rows of these projects (a portfolio or project node).
    Projects(BTreeSet<Uuid>),
    /// Rows of these sessions (an area node's selected Epics).
    Sessions(BTreeSet<Uuid>),
}

impl FleetScope {
    fn admits(&self, project: Option<Uuid>, session: Option<Uuid>) -> bool {
        match self {
            Self::All => true,
            Self::Projects(projects) => project.is_some_and(|id| projects.contains(&id)),
            Self::Sessions(sessions) => session.is_some_and(|id| sessions.contains(&id)),
        }
    }
}

impl Store {
    pub fn fleet_overview(&self, now: DateTime<Utc>) -> Result<FleetOverview> {
        self.fleet_overview_scoped(now, &FleetScope::All)
    }

    /// #1232's aggregation with the rows outside `scope` skipped. The row
    /// bounds apply before the filter, so a truncated fleet marks every
    /// scope's rollup truncated.
    pub fn fleet_overview_scoped(
        &self,
        now: DateTime<Utc>,
        scope: &FleetScope,
    ) -> Result<FleetOverview> {
        let mut snapshot = FleetOverview {
            as_of: now,
            agents: vec![],
            groups: vec![],
            totals: Default::default(),
            agents_truncated: false,
            usage_truncated: false,
        };
        let mut groups = BTreeMap::new();
        // Active sessions have no lower age cutoff: a long-running agent remains visible.
        // The indexed status predicate, snapshot upper bound and LIMIT bound this read.
        let sql = format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE id IN ({ACTIVE_SQL}) ORDER BY updated_at DESC, id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![timestamp(now), AGENT_LIMIT + 1], map_session_row)?;
        let mut scanned = 0;
        for row in rows {
            if scanned == AGENT_LIMIT {
                snapshot.agents_truncated = true;
                break;
            }
            scanned += 1;
            let session = row?.into_session()?;
            if !scope.admits(session.project_id, Some(session.id)) {
                continue;
            }
            let (project,role,start):(String,String,Option<String>)=self.conn.query_row(
                "SELECT COALESCE(p.name,'Unassigned'),
                 CASE WHEN EXISTS(SELECT 1 FROM global_manager_grants gm WHERE gm.state='active' AND gm.seat_session_id=s.id) THEN 'manager'
                      WHEN EXISTS(SELECT 1 FROM harness_manager_scopes h WHERE h.project_id=s.project_id AND h.manager_session_id=s.id)
                      THEN 'manager' WHEN parent.lead_session_id=s.id THEN 'lead'
                      ELSE COALESCE(s.agent_role,'worker') END,
                 (SELECT COALESCE(started_at,created_at) FROM model_invocations
                  WHERE session_id=s.id AND status='running' AND created_at<=?2 ORDER BY created_at DESC LIMIT 1)
                 FROM sessions s LEFT JOIN projects p ON p.id=s.project_id LEFT JOIN sessions parent ON parent.id=s.parent_id WHERE s.id=?1",
                params![session.id.to_string(),timestamp(now)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            let project_key = session
                .project_id
                .map(|id| id.to_string())
                .unwrap_or_default();
            let provider = format!("{:?}", session.provider);
            let model = session.model.as_deref().unwrap_or("Unknown");
            for (dim, key, label) in [
                ("project", project_key.as_str(), project.as_str()),
                ("provider", provider.as_str(), provider.as_str()),
                ("model", model, model),
            ] {
                group(&mut groups, dim, key, label).active += 1;
            }
            snapshot.agents.push(FleetAgent {
                session,
                project,
                role,
                turn_started_at: start
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|t| t.with_timezone(&Utc)),
            });
        }
        let mut stmt = self.conn.prepare(USAGE_SQL)?;
        let mut rows = stmt.query(params![
            timestamp(now - Duration::hours(24)),
            timestamp(now),
            USAGE_LIMIT + 1
        ])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            if count == USAGE_LIMIT {
                snapshot.usage_truncated = true;
                break;
            }
            count += 1;
            let project: String = row.get(0)?;
            let project_name: String = row.get(1)?;
            let provider: String = row.get(2)?;
            let model: String = row.get(3)?;
            let created: String = row.get(4)?;
            let status: String = row.get(5)?;
            let input: Option<u64> = row.get(6)?;
            let output: Option<u64> = row.get(7)?;
            let read: Option<u64> = row.get(8)?;
            let write: Option<u64> = row.get(9)?;
            let cost: Option<f64> = row.get(10)?;
            let session: Option<String> = row.get(11)?;
            if !scope.admits(
                Uuid::parse_str(&project).ok(),
                session.as_deref().and_then(|id| Uuid::parse_str(id).ok()),
            ) {
                continue;
            }
            let created_at = DateTime::parse_from_rfc3339(&created).ok();
            for (w, seconds) in FLEET_WINDOWS.into_iter().enumerate() {
                if created_at.is_none_or(|at| at > now || at < now - Duration::seconds(seconds)) {
                    continue;
                }
                let accumulate = |u: &mut FleetUsage| {
                    u.invocations += 1;
                    u.errors += u64::from(status == "failed");
                    u.input += input.unwrap_or(0);
                    u.output += output.unwrap_or(0);
                    u.cache_read += read.unwrap_or(0);
                    u.cache_write += write.unwrap_or(0);
                    u.cost += cost.unwrap_or(0.0);
                    u.unknown_usage += u64::from(
                        input.is_none()
                            || output.is_none()
                            || read.is_none()
                            || write.is_none()
                            || cost.is_none(),
                    );
                };
                accumulate(&mut snapshot.totals[w]);
                for (dim, key, label) in [
                    ("project", project.as_str(), project_name.as_str()),
                    ("provider", provider.as_str(), provider.as_str()),
                    ("model", model.as_str(), model.as_str()),
                ] {
                    accumulate(&mut group(&mut groups, dim, key, label).windows[w]);
                }
            }
        }
        snapshot.groups = groups.into_values().collect();
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{Project, SessionStatus};
    use uuid::Uuid;
    fn invocation(
        store: &Store,
        project: Uuid,
        provider: &str,
        model: &str,
        at: DateTime<Utc>,
        status: &str,
        tokens: Option<i64>,
    ) {
        store.conn.execute("INSERT INTO model_invocations (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,project_id,provider,model,created_at,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,estimated_cost_usd)
        VALUES (?1,'session','cli','foreground','paid','admitted',?2,'test',?3,?4,?5,?6,?7,20,30,40,0.5)",params![Uuid::new_v4().to_string(),status,project.to_string(),provider,model,timestamp(at),tokens]).unwrap();
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn fleet_aggregates_projects_providers_models_and_exact_windows() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let projects = [Uuid::new_v4(), Uuid::new_v4()];
        for (i, id) in projects.iter().enumerate() {
            store
                .insert_project(&Project {
                    id: *id,
                    name: format!("Project {i}"),
                    path: None,
                    description: None,
                    color: Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
        }
        invocation(
            &store,
            projects[0],
            "Codex",
            "alpha",
            now - Duration::minutes(5),
            "completed",
            Some(10),
        );
        invocation(
            &store,
            projects[1],
            "Claude",
            "beta",
            now - Duration::hours(1),
            "failed",
            Some(100),
        );
        invocation(
            &store,
            projects[0],
            "Codex",
            "alpha",
            now - Duration::hours(24),
            "completed",
            None,
        );
        invocation(
            &store,
            projects[0],
            "Codex",
            "alpha",
            now - Duration::hours(24) - Duration::nanoseconds(1),
            "completed",
            Some(999),
        );
        invocation(
            &store,
            projects[0],
            "Codex",
            "alpha",
            now + Duration::seconds(1),
            "completed",
            Some(999),
        );
        invocation(
            &store,
            projects[0],
            "Codex",
            "alpha",
            now - Duration::minutes(5) - Duration::nanoseconds(1),
            "completed",
            Some(7),
        );
        let mut agent = crate::test_support::test_session(Uuid::new_v4(), "/tmp/fleet".into());
        agent.project_id = Some(projects[1]);
        agent.status = SessionStatus::Running;
        agent.updated_at = now;
        agent.model = Some("beta".into());
        store.insert_session(&agent).unwrap();
        let result = store.fleet_overview(now).unwrap();
        assert_eq!(result.agents.len(), 1);
        assert_eq!(result.agents[0].session.id, agent.id);
        assert_eq!(
            result
                .totals
                .iter()
                .map(|u| u.invocations)
                .collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        assert_eq!(result.totals[0].tokens_per_minute(300), 20.0);
        assert_eq!(result.totals[0].cost_per_hour(300), 6.0);
        assert_eq!(result.totals[2].input, 117);
        assert_eq!(result.totals[2].unknown_usage, 1);
        assert_eq!(result.totals[2].errors, 1);
        assert_eq!(result.totals[2].error_pct(), 25.0);
        let beta = result
            .groups
            .iter()
            .find(|g| g.dimension == "model" && g.key == "beta")
            .unwrap();
        assert_eq!(beta.active, 1);
        assert_eq!(beta.windows[0].invocations, 0);
        assert_eq!(beta.windows[1].errors, 1);
        assert_eq!(
            result
                .groups
                .iter()
                .filter(|g| g.dimension == "project")
                .count(),
            2
        );
        assert_eq!(
            result
                .groups
                .iter()
                .filter(|g| g.dimension == "provider")
                .map(|g| g.windows[2].invocations)
                .sum::<u64>(),
            4
        );
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn fleet_queries_use_range_indexes_and_report_limits() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        for (sql, values, expected) in [
            (
                ACTIVE_SQL,
                vec![timestamp(now), "2049".into()],
                "idx_fleet_active_sessions",
            ),
            (
                USAGE_SQL,
                vec![
                    timestamp(now - Duration::hours(24)),
                    timestamp(now),
                    "50001".into(),
                ],
                "idx_fleet_invocations_created",
            ),
        ] {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let plan = stmt
                .query_map(rusqlite::params_from_iter(values), |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
                .join("\n");
            assert!(plan.contains(expected), "{plan}");
            if sql == ACTIVE_SQL {
                assert!(
                    !plan.contains("TEMP B-TREE"),
                    "active snapshot must stream from its partial index: {plan}"
                );
            }
        }
        let s = crate::test_support::test_session(Uuid::new_v4(), "/tmp/fleet".into());
        store.insert_session(&s).unwrap();
        store.conn.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < ?1)
            INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,created_at)
            SELECT printf('%08x-0000-4000-8000-000000000000',x),'session','cli','foreground','paid','admitted','completed','test',?2 FROM n",params![USAGE_LIMIT+1,timestamp(now)]).unwrap();
        let snapshot = store.fleet_overview(now).unwrap();
        assert!(snapshot.usage_truncated);
        assert_eq!(snapshot.totals[0].invocations, USAGE_LIMIT as u64);
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn fleet_active_snapshot_keeps_old_agents_and_excludes_containers() {
        use rsi_common::types::SessionKind;
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let mut epic = crate::test_support::test_session(Uuid::new_v4(), "/tmp/fleet".into());
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        epic.updated_at = now;
        store.insert_session(&epic).unwrap();
        let mut lead = crate::test_support::test_session(Uuid::new_v4(), "/tmp/fleet".into());
        lead.parent_id = Some(epic.id);
        lead.status = SessionStatus::WaitingApproval;
        lead.updated_at = now - Duration::days(40);
        store.insert_session(&lead).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?1 WHERE id=?2",
                params![lead.id.to_string(), epic.id.to_string()],
            )
            .unwrap();
        let snapshot = store.fleet_overview(now).unwrap();
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(snapshot.agents[0].session.id, lead.id);
        assert_eq!(snapshot.agents[0].role, "lead");
    }
}
