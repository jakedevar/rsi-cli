//! #3316 (slice B of #3314): the bounded, operator-only global dashboard read.
//!
//! Every statement is capped and index-driven (see
//! `dashboard_queries_use_their_indexes`); a capped read marks the snapshot
//! incomplete and is never silently partial. Contract:
//! `docs/agents/rsid-rpc-contracts.md` ("Global dashboard").
use super::Store;
use crate::error::Result;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::global_dashboard::*;
use rusqlite::params;
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Row caps of one dashboard read.
#[derive(Debug, Clone, Copy)]
pub struct DashboardLimits {
    pub projects: usize,
    pub sessions: usize,
    pub invocations: usize,
    /// Issues read per project.
    pub issues: usize,
    /// Merge-queue entries read per outcome.
    pub landings: usize,
}

impl DashboardLimits {
    pub fn for_window(window: DashboardWindow) -> Self {
        Self {
            projects: 256,
            sessions: 4096,
            invocations: match window {
                DashboardWindow::Hour => 50_000,
                DashboardWindow::Day => 100_000,
                DashboardWindow::Week => 200_000,
            },
            issues: 50_000,
            landings: 50_000,
        }
    }
}

/// Live leaf sessions through the partial index `idx_fleet_active_sessions`
/// (the WHERE and ORDER BY are the fleet read's), then their role.
const SESSIONS_SQL: &str = "SELECT COALESCE(s.project_id,''), s.provider, COALESCE(s.model,'Unknown'), s.effort,
    CASE WHEN EXISTS(SELECT 1 FROM global_manager_grants gm WHERE gm.state='active' AND gm.seat_session_id=s.id) THEN 'manager'
         WHEN EXISTS(SELECT 1 FROM harness_manager_scopes h WHERE h.project_id=s.project_id AND h.manager_session_id=s.id) THEN 'manager'
         WHEN parent.lead_session_id=s.id THEN 'lead'
         WHEN lower(COALESCE(s.agent_role,'')) LIKE '%review%' THEN 'reviewer'
         WHEN s.parent_id IS NOT NULL THEN 'worker'
         ELSE 'other' END
    FROM (SELECT id, project_id, provider, model, effort, agent_role, parent_id FROM sessions
          WHERE status IN ('Starting','Running','WaitingApproval') AND updated_at <= ?1
            AND COALESCE(session_kind,'Standard') NOT IN ('Group','Epic')
          ORDER BY updated_at DESC, id LIMIT ?2) s
    LEFT JOIN sessions parent ON parent.id = s.parent_id";
/// Invocations of the window through `idx_fleet_invocations_created`.
///
/// The cost is each invocation's OWN cost (#3534): a session launch/resume row
/// stores the session's cumulative figure (`model_control::cost`, #2593), so
/// summing the raw column over a window counted a long session once per turn
/// (a 10x fleet overstatement). The increment table is scoped to the sessions
/// that have a row in the window, with their full history as the predecessor
/// window; a row with no session keeps its recorded cost.
const USAGE_SQL: &str = concat!(
    "WITH ids(id) AS (SELECT DISTINCT session_id FROM model_invocations
        WHERE created_at >= ?1 AND created_at <= ?2 AND session_id IS NOT NULL)
    SELECT COALESCE(w.project_id,''), COALESCE(w.provider,'Unknown'), COALESCE(w.model,'Unknown'), w.status,
    w.input_tokens, w.output_tokens, w.cache_read_tokens, w.cache_creation_tokens, w.reasoning_tokens,
    ",
    "CASE WHEN rsi_billing_mode(w.provider,w.model,(SELECT json_group_object(key,value) FROM daemon_settings WHERE key IN ('claude_billing_mode','codex_billing_mode','codexappserver_billing_mode','pioneer_billing_mode','antigravity_billing_mode','billing_login.claude','billing_login.codex')))='subscription' THEN 0.0 ELSE CASE WHEN w.session_id IS NULL THEN w.estimated_cost_usd ELSE ci.cost_increment_usd END END",
    "
    FROM (SELECT id, session_id, project_id, provider, model, status, created_at, input_tokens, output_tokens,
                 cache_read_tokens, cache_creation_tokens, reasoning_tokens, estimated_cost_usd
          FROM model_invocations WHERE created_at >= ?1 AND created_at <= ?2
          ORDER BY created_at DESC, id LIMIT ?3) w
    LEFT JOIN (",
    crate::model_control::cost::cost_increment_for_ids_sql!(),
    ") ci ON ci.invocation_id = w.id"
);
const HOME_SQL: &str = "SELECT id, name, path FROM projects WHERE id = ?1";
const PROJECTS_SQL: &str =
    "SELECT id, name, path FROM projects WHERE id != ?1 ORDER BY name LIMIT ?2";
const PM_SEATS_SQL: &str =
    "SELECT h.project_id, h.manager_session_id, s.status FROM harness_manager_scopes h
    JOIN sessions s ON s.id = h.manager_session_id";
/// One project's Issue counts, from `idx_issues_project_display` (capped).
const ISSUES_SQL: &str = "SELECT count(*),
    COALESCE(SUM(status='Open' AND archived_at IS NULL),0),
    COALESCE(SUM(status='InProgress' AND archived_at IS NULL),0),
    COALESCE(SUM(created_at >= ?2 AND created_at <= ?3),0),
    COALESCE(SUM(status='Closed' AND closed_at >= ?2 AND closed_at <= ?3),0),
    COALESCE(SUM(status IN ('Open','InProgress') AND archived_at IS NULL
        AND EXISTS(SELECT 1 FROM json_each(i.labels) l WHERE l.value='operator-request')),0)
    FROM (SELECT status, archived_at, created_at, closed_at, labels FROM issues
          WHERE project_id = ?1 ORDER BY display_number DESC LIMIT ?4) i";
/// Merge-queue entries of one outcome through `rolling_queue_by_state`.
const LANDINGS_SQL: &str = "SELECT COALESCE(project_id,''), finished_at FROM rolling_queue_entries
    WHERE state = ?1 ORDER BY sequence DESC LIMIT ?2";

fn timestamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[derive(Default)]
struct AgentAcc {
    managers: u64,
    leads: u64,
    workers: u64,
    reviewers: u64,
    other: u64,
    by_provider: BTreeMap<String, u64>,
    by_model: BTreeMap<(String, String, Option<String>), u64>,
}

impl AgentAcc {
    fn add(&mut self, provider: &str, model: &str, effort: Option<String>, role: &str) {
        match role {
            "manager" => self.managers += 1,
            "lead" => self.leads += 1,
            "reviewer" => self.reviewers += 1,
            "worker" => self.workers += 1,
            _ => self.other += 1,
        }
        *self.by_provider.entry(provider.into()).or_default() += 1;
        *self
            .by_model
            .entry((provider.into(), model.into(), effort))
            .or_default() += 1;
    }

    fn into_agents(self) -> DashboardAgents {
        DashboardAgents {
            total: self.managers + self.leads + self.workers + self.reviewers + self.other,
            managers: self.managers,
            leads: self.leads,
            workers: self.workers,
            reviewers: self.reviewers,
            other: self.other,
            by_provider: self
                .by_provider
                .into_iter()
                .map(|(provider, count)| DashboardProviderCount { provider, count })
                .collect(),
            by_model: self
                .by_model
                .into_iter()
                .map(|((provider, model, effort), count)| DashboardAgentGroup {
                    provider,
                    model,
                    effort,
                    count,
                })
                .collect(),
        }
    }
}

#[derive(Default)]
struct UsageAcc {
    usage: DashboardUsage,
    by_model: BTreeMap<(String, String), DashboardUsage>,
}

impl UsageAcc {
    fn into_parts(self) -> (DashboardUsage, Vec<DashboardModelUsage>) {
        (
            self.usage,
            self.by_model
                .into_iter()
                .map(|((provider, model), usage)| DashboardModelUsage {
                    provider,
                    model,
                    usage,
                })
                .collect(),
        )
    }
}

impl Store {
    /// The global dashboard at `now` over `window`; `home` is the home
    /// project (listed first when it exists).
    ///
    /// # Errors
    /// A persistence error.
    pub fn global_dashboard(
        &self,
        now: DateTime<Utc>,
        window: DashboardWindow,
        home: Option<Uuid>,
    ) -> Result<GlobalDashboard> {
        self.global_dashboard_with(now, window, home, DashboardLimits::for_window(window))
    }

    pub fn global_dashboard_with(
        &self,
        now: DateTime<Utc>,
        window: DashboardWindow,
        home: Option<Uuid>,
        limits: DashboardLimits,
    ) -> Result<GlobalDashboard> {
        let start = now - Duration::seconds(window.seconds());
        let (start_text, now_text) = (timestamp(start), timestamp(now));
        let mut reasons: Vec<String> = vec![];

        // Projects: the home project first, then by name, capped.
        let home_text = home.map(|id| id.to_string()).unwrap_or_default();
        let mut listed: Vec<(Uuid, String, Option<String>)> = vec![];
        let project_row = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        };
        let mut home_found = false;
        if home.is_some() {
            let mut stmt = self.conn.prepare(HOME_SQL)?;
            for row in stmt.query_map(params![home_text], project_row)? {
                let (id, name, path) = row?;
                if let Ok(id) = Uuid::parse_str(&id) {
                    listed.push((id, name, path));
                    home_found = true;
                }
            }
        }
        let mut projects_incomplete = false;
        {
            let mut stmt = self.conn.prepare(PROJECTS_SQL)?;
            let rows =
                stmt.query_map(params![home_text, limits.projects as i64 + 1], project_row)?;
            for (seen, row) in rows.enumerate() {
                if seen == limits.projects {
                    projects_incomplete = true;
                    break;
                }
                let (id, name, path) = row?;
                if let Ok(id) = Uuid::parse_str(&id) {
                    listed.push((id, name, path));
                }
            }
        }
        if projects_incomplete {
            reasons.push("projects".into());
        }

        // Live leaf sessions.
        let mut agents: HashMap<String, AgentAcc> = HashMap::new();
        let mut agents_total = AgentAcc::default();
        {
            let mut stmt = self.conn.prepare(SESSIONS_SQL)?;
            let mut rows = stmt.query(params![now_text, limits.sessions as i64 + 1])?;
            let mut seen = 0;
            while let Some(row) = rows.next()? {
                if seen == limits.sessions {
                    reasons.push("sessions".into());
                    break;
                }
                seen += 1;
                let project: String = row.get(0)?;
                let provider: String = row.get(1)?;
                let model: String = row.get(2)?;
                let effort: Option<String> = row.get(3)?;
                let role: String = row.get(4)?;
                agents_total.add(&provider, &model, effort.clone(), &role);
                if !project.is_empty() {
                    agents
                        .entry(project)
                        .or_default()
                        .add(&provider, &model, effort, &role);
                }
            }
        }

        // Invocations of the window.
        let mut usage: HashMap<String, UsageAcc> = HashMap::new();
        let mut usage_total = UsageAcc::default();
        {
            let mut stmt = self.conn.prepare(USAGE_SQL)?;
            let mut rows =
                stmt.query(params![start_text, now_text, limits.invocations as i64 + 1])?;
            let mut seen = 0;
            while let Some(row) = rows.next()? {
                if seen == limits.invocations {
                    reasons.push("invocations".into());
                    break;
                }
                seen += 1;
                let project: String = row.get(0)?;
                let provider: String = row.get(1)?;
                let model: String = row.get(2)?;
                let status: String = row.get(3)?;
                let one = DashboardUsage {
                    invocations: 1,
                    errors: u64::from(status == "failed"),
                    input_tokens: row.get::<_, Option<u64>>(4)?.unwrap_or(0),
                    output_tokens: row.get::<_, Option<u64>>(5)?.unwrap_or(0),
                    cache_read_tokens: row.get::<_, Option<u64>>(6)?.unwrap_or(0),
                    cache_creation_tokens: row.get::<_, Option<u64>>(7)?.unwrap_or(0),
                    reasoning_tokens: row.get::<_, Option<u64>>(8)?.unwrap_or(0),
                    estimated_cost_usd: row.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                };
                let key = (provider, model);
                usage_total.usage.add(&one);
                usage_total
                    .by_model
                    .entry(key.clone())
                    .or_default()
                    .add(&one);
                if !project.is_empty() {
                    let acc = usage.entry(project).or_default();
                    acc.usage.add(&one);
                    acc.by_model.entry(key).or_default().add(&one);
                }
            }
        }

        // Landings: entries that finished in the window, by outcome.
        let mut landings: HashMap<String, DashboardLandings> = HashMap::new();
        let mut landings_total = DashboardLandings::default();
        let mut landings_capped = false;
        for state in ["published", "refused"] {
            let mut stmt = self.conn.prepare(LANDINGS_SQL)?;
            let mut rows = stmt.query(params![state, limits.landings as i64 + 1])?;
            let mut seen = 0;
            while let Some(row) = rows.next()? {
                if seen == limits.landings {
                    landings_capped = true;
                    break;
                }
                seen += 1;
                let project: String = row.get(0)?;
                let finished: Option<String> = row.get(1)?;
                let in_window = finished
                    .and_then(|at| DateTime::parse_from_rfc3339(&at).ok())
                    .map(|at| at.with_timezone(&Utc))
                    .is_some_and(|at| at >= start && at <= now);
                if !in_window {
                    continue;
                }
                let mut one = DashboardLandings::default();
                if state == "published" {
                    one.published = 1;
                } else {
                    one.refused = 1;
                }
                landings_total.add(&one);
                if !project.is_empty() {
                    landings.entry(project).or_default().add(&one);
                }
            }
        }
        if landings_capped {
            reasons.push("landings".into());
        }

        // PM seats.
        let mut seats: HashMap<String, DashboardPmSeat> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(PM_SEATS_SQL)?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (project, session, status) = row?;
                if let Ok(session_id) = Uuid::parse_str(&session) {
                    seats.insert(project, DashboardPmSeat { session_id, status });
                }
            }
        }

        // Per-project Issues and rolling health.
        let mut totals_issues = DashboardIssues::default();
        let mut projects = Vec::with_capacity(listed.len());
        for (index, (id, name, path)) in listed.into_iter().enumerate() {
            let key = id.to_string();
            let (count, issues) = self.conn.query_row(
                ISSUES_SQL,
                params![key, start_text, now_text, limits.issues as i64],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        DashboardIssues {
                            open: row.get(1)?,
                            in_progress: row.get(2)?,
                            created_in_window: row.get(3)?,
                            closed_in_window: row.get(4)?,
                            open_operator_requests: row.get(5)?,
                        },
                    ))
                },
            )?;
            if count >= limits.issues as u64 {
                reasons.push(format!("issues:{id}"));
            }
            totals_issues.add(&issues);
            let health = self.rolling_health_for_project(id, now)?;
            let rolling =
                (health.qa_green.is_some() || health.open_qa_issues_total > 0).then(|| {
                    DashboardRollingHealth {
                        qa_green_sha: health.qa_green.map(|green| green.sha),
                        qa_green_age_secs: health.qa_green_age_secs,
                        open_qa_issues: health.open_qa_issues_total,
                    }
                });
            let (project_usage, usage_by_model) =
                usage.remove(&key).unwrap_or_default().into_parts();
            projects.push(ProjectDashboard {
                id,
                name,
                path,
                is_home: home_found && index == 0,
                agents: agents.remove(&key).unwrap_or_default().into_agents(),
                pm_seat: seats.remove(&key),
                usage: project_usage,
                usage_by_model,
                issues,
                landings: landings.remove(&key).unwrap_or_default(),
                rolling,
            });
        }

        let (total_usage, total_by_model) = usage_total.into_parts();
        let incomplete = !reasons.is_empty();
        Ok(GlobalDashboard {
            as_of: now,
            window: window.label().into(),
            window_seconds: window.seconds(),
            projects,
            totals: DashboardTotals {
                agents: agents_total.into_agents(),
                usage: total_usage,
                usage_by_model: total_by_model,
                issues: totals_issues,
                landings: landings_total,
                incomplete,
            },
            projects_incomplete,
            incomplete_reasons: reasons,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Dollar-accounting fixtures use API logins. Subscription coverage lives
    // in provider_billing::tests and never treats CLI list prices as bills.
    fn metered_store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .set_daemon_setting("claude_billing_mode", "metered_api")
            .unwrap();
        store
            .set_daemon_setting("codex_billing_mode", "metered_api")
            .unwrap();
        store
    }

    use rsi_common::types::{
        IssueStatus, NewIssue, Project, Session, SessionKind, SessionProvider, SessionStatus,
    };

    fn project(store: &Store, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id,
                name: name.into(),
                path: Some(format!("/tmp/{name}").into()),
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        id
    }

    #[allow(clippy::too_many_arguments)]
    fn session(
        store: &Store,
        project: Uuid,
        status: SessionStatus,
        provider: SessionProvider,
        model: &str,
        effort: Option<&str>,
        parent: Option<Uuid>,
        role: Option<&str>,
    ) -> Session {
        let mut s = crate::test_support::test_session(Uuid::new_v4(), "/tmp/dash".into());
        s.project_id = Some(project);
        s.status = status;
        s.provider = provider;
        s.model = Some(model.into());
        s.effort = effort.map(Into::into);
        s.parent_id = parent;
        s.agent_role = role.map(Into::into);
        store.insert_session(&s).unwrap();
        s
    }

    struct Inv {
        project: Option<Uuid>,
        provider: &'static str,
        model: &'static str,
        status: &'static str,
        at: DateTime<Utc>,
        tokens: [u64; 5],
        cost: f64,
    }

    fn invocation(store: &Store, inv: &Inv) {
        store.conn.execute(
            "INSERT INTO model_invocations (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,project_id,provider,model,created_at,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,reasoning_tokens,estimated_cost_usd)
             VALUES (?1,'session','cli','foreground','paid','admitted',?2,'test',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                Uuid::new_v4().to_string(),
                inv.status,
                inv.project.map(|p| p.to_string()),
                inv.provider,
                inv.model,
                timestamp(inv.at),
                inv.tokens[0],
                inv.tokens[1],
                inv.tokens[2],
                inv.tokens[3],
                inv.tokens[4],
                inv.cost
            ],
        ).unwrap();
    }

    fn issue(
        store: &Store,
        project: Uuid,
        title: &str,
        labels: &[&str],
    ) -> rsi_common::types::Issue {
        store
            .create_issue(&NewIssue {
                project_id: project,
                title: title.into(),
                body: String::new(),
                priority: Some(2),
                labels: labels.iter().map(|l| (*l).to_string()).collect(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap()
    }

    fn queue_entry(
        store: &Store,
        project: Uuid,
        state: &str,
        finished: DateTime<Utc>,
        n: u32,
    ) -> String {
        let id = Uuid::new_v4().to_string();
        store.conn.execute(
            "INSERT INTO rolling_queue_entries(id,project_id,repo_path,source_commit,source_session_id,binding,hot_files_json,test_filters_json,state,idempotency_key,enqueued_at,finished_at,row_version)
             VALUES (?1,?2,'/tmp/repo',?3,?4,'unbound','[]','[]',?5,?6,?7,?7,1)",
            params![
                id,
                project.to_string(),
                format!("{n:040x}"),
                Uuid::new_v4().to_string(),
                state,
                format!("key-{n}"),
                timestamp(finished)
            ],
        ).unwrap();
        id
    }

    struct Fixture {
        store: Store,
        now: DateTime<Utc>,
        home: Uuid,
        other: Uuid,
        manager: Session,
    }

    fn fixture() -> Fixture {
        let store = metered_store();
        // The home project sorts after the other by name: home still lists first.
        let home = project(&store, "zeta-home");
        let other = project(&store, "alpha-other");

        // Home project: a manager seat, an Epic with a lead, a worker, a
        // reviewer, a parentless session, and finished sessions.
        let manager = session(
            &store,
            home,
            SessionStatus::Running,
            SessionProvider::Claude,
            "m1",
            Some("high"),
            None,
            Some("manager"),
        );
        store.conn.execute(
            "INSERT INTO harness_manager_scopes(project_id,manager_session_id,epic_ids_json,row_version,updated_at) VALUES (?1,?2,'[]',1,?3)",
            params![home.to_string(), manager.id.to_string(), timestamp(Utc::now())],
        ).unwrap();
        let mut epic = crate::test_support::test_session(Uuid::new_v4(), "/tmp/dash".into());
        epic.project_id = Some(home);
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        store.insert_session(&epic).unwrap();
        let lead = session(
            &store,
            home,
            SessionStatus::Running,
            SessionProvider::Claude,
            "m1",
            Some("high"),
            Some(epic.id),
            Some("Epic lead"),
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?1 WHERE id=?2",
                params![lead.id.to_string(), epic.id.to_string()],
            )
            .unwrap();
        session(
            &store,
            home,
            SessionStatus::Running,
            SessionProvider::Claude,
            "m1",
            Some("high"),
            Some(epic.id),
            Some("worker"),
        );
        session(
            &store,
            home,
            SessionStatus::WaitingApproval,
            SessionProvider::Codex,
            "m3",
            None,
            Some(epic.id),
            Some("Independent Reviewer"),
        );
        session(
            &store,
            home,
            SessionStatus::Starting,
            SessionProvider::Codex,
            "m3",
            None,
            None,
            None,
        );
        for status in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Archived,
        ] {
            session(
                &store,
                home,
                status,
                SessionProvider::Claude,
                "m1",
                None,
                Some(epic.id),
                Some("worker"),
            );
        }
        // Other project: one running worker and a finished one.
        let o_epic = {
            let mut e = crate::test_support::test_session(Uuid::new_v4(), "/tmp/dash".into());
            e.project_id = Some(other);
            e.session_kind = SessionKind::Group;
            e.status = SessionStatus::Running;
            store.insert_session(&e).unwrap();
            e
        };
        session(
            &store,
            other,
            SessionStatus::Running,
            SessionProvider::Codex,
            "m2",
            Some("xhigh"),
            Some(o_epic.id),
            Some("worker"),
        );
        session(
            &store,
            other,
            SessionStatus::Interrupted,
            SessionProvider::Codex,
            "m2",
            None,
            Some(o_epic.id),
            Some("worker"),
        );

        let now = Utc::now() + Duration::seconds(5);
        let inv = |project, provider, model, status, at, tokens, cost| {
            invocation(
                &store,
                &Inv {
                    project,
                    provider,
                    model,
                    status,
                    at,
                    tokens,
                    cost,
                },
            );
        };
        // Home: inside 1h, inside 24h only, inside 7d only, outside 7d.
        inv(
            Some(home),
            "Claude",
            "m1",
            "completed",
            now - Duration::minutes(5),
            [10, 20, 30, 40, 5],
            0.5,
        );
        inv(
            Some(home),
            "Claude",
            "m1",
            "failed",
            now - Duration::hours(2),
            [1, 2, 3, 4, 6],
            0.25,
        );
        inv(
            Some(home),
            "Codex",
            "m3",
            "completed",
            now - Duration::hours(30),
            [100, 200, 300, 400, 7],
            2.0,
        );
        inv(
            Some(home),
            "Codex",
            "m3",
            "completed",
            now - Duration::days(8),
            [999, 999, 999, 999, 999],
            99.0,
        );
        // Other project and a no-project invocation.
        inv(
            Some(other),
            "Codex",
            "m2",
            "completed",
            now - Duration::minutes(10),
            [1000, 2000, 3000, 4000, 8],
            1.5,
        );
        inv(
            None,
            "Claude",
            "m1",
            "completed",
            now - Duration::minutes(20),
            [7, 7, 7, 7, 7],
            0.125,
        );
        // After the snapshot instant: never counted.
        inv(
            Some(home),
            "Claude",
            "m1",
            "completed",
            now + Duration::seconds(30),
            [5000, 5000, 5000, 5000, 5000],
            50.0,
        );

        // Issues. Home: open + operator-request, in progress + operator-request,
        // closed in window, closed outside the window, cancelled, one qa.
        issue(&store, home, "open request", &["operator-request"]);
        let progress = issue(
            &store,
            home,
            "progress request",
            &["operator-request", "bug"],
        );
        store
            .update_issue_status(progress.id, IssueStatus::InProgress)
            .unwrap();
        let done = issue(&store, home, "done", &[]);
        store
            .update_issue_status(done.id, IssueStatus::Closed)
            .unwrap();
        let old = issue(&store, home, "old done", &["operator-request"]);
        store
            .update_issue_status(old.id, IssueStatus::Closed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE issues SET created_at=?1, closed_at=?1 WHERE id=?2",
                params![timestamp(now - Duration::days(10)), old.id.to_string()],
            )
            .unwrap();
        let gone = issue(&store, home, "cancelled", &[]);
        store
            .update_issue_status(gone.id, IssueStatus::Cancelled)
            .unwrap();
        issue(&store, home, "red qa", &["qa"]);
        issue(&store, other, "other open", &[]);
        issue(&store, other, "other request", &["operator-request"]);

        // Landings. Home: published 1h ago, refused 5 min ago, published 10 days ago.
        let landed = queue_entry(&store, home, "published", now - Duration::minutes(50), 1);
        queue_entry(&store, home, "refused", now - Duration::minutes(5), 2);
        queue_entry(&store, home, "published", now - Duration::days(10), 3);
        queue_entry(&store, other, "published", now - Duration::hours(3), 4);
        queue_entry(&store, other, "failed", now - Duration::minutes(5), 5);
        let batch = Uuid::new_v4().to_string();
        store.conn.execute(
            "INSERT INTO rolling_queue_batches(id,project_id,state,member_ids_json,member_count,created_at,row_version) VALUES (?1,?2,'published','[]',1,?3,1)",
            params![batch, home.to_string(), timestamp(now)],
        ).unwrap();
        store.conn.execute(
            "INSERT INTO rolling_queue_events(batch_id,kind,entry_id,detail_json,at) VALUES (?1,'bounded_qa_finished',?2,?3,?4)",
            params![batch, landed, r#"{"passed":true,"sha":"abcdef0123456789abcdef0123456789abcdef01"}"#, timestamp(now - Duration::minutes(30))],
        ).unwrap();
        Fixture {
            store,
            now,
            home,
            other,
            manager,
        }
    }

    /// The fixture projects in listed order (the store seeds a default project too).
    fn listed(d: &GlobalDashboard, f: &Fixture) -> Vec<Uuid> {
        d.projects
            .iter()
            .map(|p| p.id)
            .filter(|id| *id == f.home || *id == f.other)
            .collect()
    }

    fn by_id(d: &GlobalDashboard, id: Uuid) -> &ProjectDashboard {
        d.projects.iter().find(|p| p.id == id).unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn dashboard_counts_agents_by_role_provider_and_model() {
        let f = fixture();
        let d = f
            .store
            .global_dashboard(f.now, DashboardWindow::Day, Some(f.home))
            .unwrap();
        assert_eq!(d.window, "24h");
        assert_eq!(d.projects[0].id, f.home, "the home project lists first");
        assert_eq!(listed(&d, &f), vec![f.home, f.other]);
        assert!(by_id(&d, f.home).is_home);
        assert!(!by_id(&d, f.other).is_home);
        assert_eq!(by_id(&d, f.home).path.as_deref(), Some("/tmp/zeta-home"));

        let home = &by_id(&d, f.home).agents;
        // Live leaves only: the Epic container and the finished sessions do not count.
        assert_eq!(
            (
                home.total,
                home.managers,
                home.leads,
                home.workers,
                home.reviewers,
                home.other
            ),
            (5, 1, 1, 1, 1, 1)
        );
        assert_eq!(
            home.by_provider,
            vec![
                DashboardProviderCount {
                    provider: "Claude".into(),
                    count: 3
                },
                DashboardProviderCount {
                    provider: "Codex".into(),
                    count: 2
                },
            ]
        );
        assert_eq!(
            home.by_model,
            vec![
                DashboardAgentGroup {
                    provider: "Claude".into(),
                    model: "m1".into(),
                    effort: Some("high".into()),
                    count: 3
                },
                DashboardAgentGroup {
                    provider: "Codex".into(),
                    model: "m3".into(),
                    effort: None,
                    count: 2
                },
            ]
        );
        let other = &by_id(&d, f.other).agents;
        assert_eq!((other.total, other.workers), (1, 1));
        assert_eq!(other.by_model[0].effort.as_deref(), Some("xhigh"));
        assert_eq!(d.totals.agents.total, 6);
        assert_eq!(d.totals.agents.workers, 2);

        let seat = by_id(&d, f.home).pm_seat.as_ref().unwrap();
        assert_eq!(
            (seat.session_id, seat.status.as_str()),
            (f.manager.id, "Running")
        );
        assert!(by_id(&d, f.other).pm_seat.is_none());
    }

    /// #3534: a session's launch/resume rows each store the session's
    /// CUMULATIVE cost, so the dashboard counts each row's own increment. A
    /// window opening mid-session charges the first in-window row the growth
    /// over its predecessor outside the window, not the whole running total.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn dashboard_cost_counts_each_session_row_once_not_its_running_total() {
        let f = fixture();
        let s = session(
            &f.store,
            f.home,
            SessionStatus::Completed,
            SessionProvider::Claude,
            "m-cum",
            None,
            None,
            None,
        );
        for (age, cost) in [
            (Duration::hours(3), 1.0),
            (Duration::hours(2), 2.5),
            (Duration::minutes(30), 4.0),
            (Duration::minutes(10), 4.5),
        ] {
            f.store
                .conn
                .execute(
                    "INSERT INTO model_invocations (id,session_id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,project_id,provider,model,created_at,estimated_cost_usd)
                     VALUES (?1,?2,'session.continue.resume','cli','foreground','paid','admitted','completed','test',?3,'Claude','m-cum',?4,?5)",
                    params![
                        Uuid::new_v4().to_string(),
                        s.id.to_string(),
                        f.home.to_string(),
                        timestamp(f.now - age),
                        cost
                    ],
                )
                .unwrap();
        }
        let cost_of = |window| {
            let d = f
                .store
                .global_dashboard(f.now, window, Some(f.home))
                .unwrap();
            d.totals
                .usage_by_model
                .iter()
                .find(|m| m.model == "m-cum")
                .map(|m| (m.usage.invocations, m.usage.estimated_cost_usd))
        };
        // Last hour: rows at 4.0 and 4.5 cost 1.5 and 0.5 (2.0), not 8.5.
        assert_eq!(cost_of(DashboardWindow::Hour), Some((2, 2.0)));
        // Last day: the whole session, 4.5 in total, not 12.0.
        assert_eq!(cost_of(DashboardWindow::Day), Some((4, 4.5)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn dashboard_sums_tokens_inside_the_window_only() {
        let f = fixture();
        let usage = |window| {
            let d = f
                .store
                .global_dashboard(f.now, window, Some(f.home))
                .unwrap();
            (
                by_id(&d, f.home).usage.clone(),
                by_id(&d, f.other).usage.clone(),
                d.totals.usage.clone(),
                d,
            )
        };
        let (home, other, total, d) = usage(DashboardWindow::Hour);
        assert_eq!(
            home,
            DashboardUsage {
                invocations: 1,
                errors: 0,
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_creation_tokens: 40,
                reasoning_tokens: 5,
                estimated_cost_usd: 0.5
            }
        );
        assert_eq!((other.invocations, other.input_tokens), (1, 1000));
        // The no-project invocation counts in the totals only.
        assert_eq!(
            (
                total.invocations,
                total.input_tokens,
                total.estimated_cost_usd
            ),
            (3, 1017, 2.125)
        );
        assert!(!d.totals.incomplete);
        assert!(d.incomplete_reasons.is_empty());

        let (home, _, total, d) = usage(DashboardWindow::Day);
        assert_eq!(
            (
                home.invocations,
                home.errors,
                home.input_tokens,
                home.reasoning_tokens
            ),
            (2, 1, 11, 11)
        );
        assert_eq!(total.invocations, 4);
        let models: Vec<_> = by_id(&d, f.home)
            .usage_by_model
            .iter()
            .map(|m| (m.provider.as_str(), m.model.as_str(), m.usage.invocations))
            .collect();
        assert_eq!(models, vec![("Claude", "m1", 2)]);

        let (home, _, total, d) = usage(DashboardWindow::Week);
        assert_eq!(
            (home.invocations, home.input_tokens, home.estimated_cost_usd),
            (3, 111, 2.75)
        );
        assert_eq!(total.invocations, 5);
        let models: Vec<_> = by_id(&d, f.home)
            .usage_by_model
            .iter()
            .map(|m| (m.provider.as_str(), m.model.as_str(), m.usage.output_tokens))
            .collect();
        assert_eq!(models, vec![("Claude", "m1", 22), ("Codex", "m3", 200)]);
        assert_eq!(d.totals.usage_by_model.len(), 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn dashboard_counts_issues_landings_and_rolling_health() {
        let f = fixture();
        let d = f
            .store
            .global_dashboard(f.now, DashboardWindow::Day, Some(f.home))
            .unwrap();
        let home = by_id(&d, f.home);
        assert_eq!(
            home.issues,
            DashboardIssues {
                open: 2,
                in_progress: 1,
                created_in_window: 5,
                closed_in_window: 1,
                open_operator_requests: 2
            }
        );
        let other = by_id(&d, f.other);
        assert_eq!(
            other.issues,
            DashboardIssues {
                open: 2,
                in_progress: 0,
                created_in_window: 2,
                closed_in_window: 0,
                open_operator_requests: 1
            }
        );
        assert_eq!(d.totals.issues.open, 4);
        assert_eq!(d.totals.issues.open_operator_requests, 3);
        assert_eq!((home.landings.published, home.landings.refused), (1, 1));
        assert_eq!((other.landings.published, other.landings.refused), (1, 0));
        assert_eq!(
            (d.totals.landings.published, d.totals.landings.refused),
            (2, 1)
        );
        let hour = f
            .store
            .global_dashboard(f.now, DashboardWindow::Hour, Some(f.home))
            .unwrap();
        assert_eq!(
            (hour.totals.landings.published, hour.totals.landings.refused),
            (1, 1)
        );
        let week = f
            .store
            .global_dashboard(f.now, DashboardWindow::Week, Some(f.home))
            .unwrap();
        assert_eq!(
            (week.totals.landings.published, week.totals.landings.refused),
            (2, 1),
            "the 10-day-old landing is outside every window"
        );

        let rolling = home.rolling.as_ref().unwrap();
        assert_eq!(
            rolling.qa_green_sha.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
        assert_eq!(rolling.qa_green_age_secs, Some(30 * 60));
        assert_eq!(rolling.open_qa_issues, 1);
        assert!(other.rolling.is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn dashboard_without_a_home_project_lists_by_name() {
        let f = fixture();
        let d = f
            .store
            .global_dashboard(f.now, DashboardWindow::Hour, None)
            .unwrap();
        assert_eq!(listed(&d, &f), vec![f.other, f.home]);
        assert!(d.projects.iter().all(|p| !p.is_home));
        let d = f
            .store
            .global_dashboard(f.now, DashboardWindow::Hour, Some(Uuid::new_v4()))
            .unwrap();
        assert!(d.projects.iter().all(|p| !p.is_home));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn dashboard_cap_marks_the_snapshot_incomplete() {
        let f = fixture();
        let big = DashboardLimits::for_window(DashboardWindow::Day);
        for (limits, reason) in [
            (
                DashboardLimits {
                    invocations: 3,
                    ..big
                },
                "invocations",
            ),
            (DashboardLimits { sessions: 5, ..big }, "sessions"),
            (DashboardLimits { projects: 0, ..big }, "projects"),
            (DashboardLimits { landings: 1, ..big }, "landings"),
        ] {
            let d = f
                .store
                .global_dashboard_with(f.now, DashboardWindow::Day, Some(f.home), limits)
                .unwrap();
            assert!(d.totals.incomplete, "{reason}");
            assert!(
                d.incomplete_reasons.iter().any(|r| r == reason),
                "{reason}: {:?}",
                d.incomplete_reasons
            );
        }
        let d = f
            .store
            .global_dashboard_with(
                f.now,
                DashboardWindow::Day,
                Some(f.home),
                DashboardLimits { projects: 0, ..big },
            )
            .unwrap();
        assert!(d.projects_incomplete);
        assert_eq!(
            d.projects.len(),
            1,
            "only the home project survives a zero cap"
        );
        let d = f
            .store
            .global_dashboard_with(
                f.now,
                DashboardWindow::Day,
                Some(f.home),
                DashboardLimits { issues: 3, ..big },
            )
            .unwrap();
        assert!(d.totals.incomplete);
        assert!(
            d.incomplete_reasons
                .iter()
                .any(|r| r == &format!("issues:{}", f.home))
        );
        // Exactly at a cap is complete.
        let d = f
            .store
            .global_dashboard_with(
                f.now,
                DashboardWindow::Day,
                Some(f.home),
                DashboardLimits {
                    invocations: 4,
                    sessions: 6,
                    ..big
                },
            )
            .unwrap();
        assert!(!d.totals.incomplete, "{:?}", d.incomplete_reasons);
        // A capped invocation read keeps the newest rows.
        let d = f
            .store
            .global_dashboard_with(
                f.now,
                DashboardWindow::Week,
                Some(f.home),
                DashboardLimits {
                    invocations: 2,
                    ..DashboardLimits::for_window(DashboardWindow::Week)
                },
            )
            .unwrap();
        assert_eq!(d.totals.usage.invocations, 2);
        assert_eq!(d.totals.usage.input_tokens, 1010);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn dashboard_queries_use_their_indexes() {
        let store = metered_store();
        let now = timestamp(Utc::now());
        let id = Uuid::new_v4().to_string();
        let plan = |sql: &str, values: Vec<String>| {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            stmt.query_map(rusqlite::params_from_iter(values), |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
        };
        let p = plan(SESSIONS_SQL, vec![now.clone(), "4097".into()]);
        assert!(p.contains("USING INDEX idx_fleet_active_sessions"), "{p}");
        assert!(!p.contains("TEMP B-TREE"), "{p}");
        let p = plan(USAGE_SQL, vec![now.clone(), now.clone(), "200001".into()]);
        assert!(
            p.contains("USING INDEX idx_fleet_invocations_created"),
            "{p}"
        );
        // The cost increment (#3534) groups the window's sessions, which needs
        // a temp tree; the reads stay indexed, never a table scan.
        assert!(!p.contains("SCAN model_invocations"), "{p}");
        let p = plan(
            ISSUES_SQL,
            vec![id.clone(), now.clone(), now.clone(), "50000".into()],
        );
        assert!(p.contains("USING INDEX idx_issues_project_display"), "{p}");
        assert!(!p.contains("TEMP B-TREE"), "{p}");
        let p = plan(LANDINGS_SQL, vec!["published".into(), "50001".into()]);
        assert!(p.contains("USING INDEX rolling_queue_by_state"), "{p}");
        assert!(!p.contains("TEMP B-TREE"), "{p}");
        let p = plan(PROJECTS_SQL, vec![id.clone(), "257".into()]);
        assert!(p.contains("USING INDEX"), "{p}");
        assert!(!p.contains("TEMP B-TREE"), "{p}");
        let p = plan(HOME_SQL, vec![id]);
        assert!(p.contains("USING INDEX"), "{p}");
    }
}
