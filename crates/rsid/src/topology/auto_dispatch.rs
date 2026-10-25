//! Daemon dispatch of ordinary ready Issues (#3390).
//!
//! A hand-built `AgentTopologyExecute` costs a project-manager turn per
//! ordinary Issue (the PM reads the Issue and builds the input). An Issue the
//! operator or PM labels [`AUTO_LABEL`] (`dispatch:auto`) is instead launched
//! by the daemon into the matching starter on the 15-second housekeeping tick,
//! as the project's current manager seat, through the very same
//! [`SessionManager::agent_topology_execute`] path a PM call takes. The seat
//! therefore sees, owns and can interrupt the execution exactly as today, its
//! settle wake still arrives, and every refusal of the ordinary admission
//! (policy, `max_created_sessions`, pause, digest) applies unchanged.
//!
//! Eligibility is a pure function ([`classify`]) over the Issue row:
//!
//! * `Open`, not archived, labelled `dispatch:auto`;
//! * not claimed by a satellite (`assignee` `satellite:*`);
//! * no label that needs a manager's judgement or a pre-merge review
//!   (migrations, credentials, IAM, network exposure): those stay with the PM;
//! * `authority`/`custody`/`lander`/`queue`/`deploy` work routes to
//!   `issue-pipeline-post-review`, everything else to `issue-pipeline`.
//!
//! Readiness (no `Open`/`InProgress` blocker) is `Store::list_ready_issues`.
//! Per project the dispatcher also requires: no live execution or Issue-bound
//! worker already holding the Issue, no earlier execution of that Issue at all
//! (a failed run belongs to the PM, never to a retry loop), host-load and
//! pressure admission, and spare capacity: executions holding a session node
//! (reserved, launching or running; not those parked at land, review or a
//! decision) stay under `min(max_active_sessions, BUILD_HEAVY_CAP)`, and total
//! open executions stay under the higher `OPEN_EXECUTIONS_CAP`. The executor's own
//! launch admission still holds the nodes while the host is loaded.
//!
//! A refusal (`creation_limit`, a policy refusal, ...) puts that Issue on a
//! ten-minute in-memory backoff so the 15 s tick does not append a ledger row
//! per pass. A restart clears it, which is bounded by the same ten minutes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rsi_common::topology_agent::AgentTopologyExecuteRequestV1;
use rsi_common::types::{Issue, IssueStatus};
use serde_json::json;
use uuid::Uuid;

use crate::error::Result;
use crate::session::SessionManager;
use crate::store::Store;
use crate::topology::agent::{AgentKnobs, Reach, TopologyCaller};
use crate::topology::starters::{self, ISSUE_PIPELINE, ISSUE_PIPELINE_POST_REVIEW};

/// The opt-in label.
pub(crate) const AUTO_LABEL: &str = "dispatch:auto";

/// Idempotency-key prefix of every dispatcher request.
pub(crate) const KEY_PREFIX: &str = "auto-dispatch:";

/// The load rule (2026-10-07): at most this many concurrent build-heavy
/// sessions per project.
pub(crate) const BUILD_HEAVY_CAP: usize = 5;

/// Open executions (any node state) one project may hold at once. Executions
/// parked at land, in review or on a decision hold no build-heavy session, so
/// they are not bound by [`BUILD_HEAVY_CAP`]; this higher ceiling only keeps a
/// backed-up merge queue from piling up unbounded work.
pub(crate) const OPEN_EXECUTIONS_CAP: usize = 30;

/// New executions one project may start in a single tick.
const PER_PROJECT_TICK_LIMIT: usize = 3;

/// How long a refused Issue is left alone.
const REFUSAL_BACKOFF: Duration = Duration::from_secs(600);

/// Labels that keep an Issue with the PM: a migration or a credential, IAM or
/// network-exposure change needs `issue-pipeline-reviewed` and a person's
/// judgement of the reviewer.
const PRE_MERGE_REVIEW_LABELS: [&str; 7] = [
    "migration",
    "migrations",
    "credential",
    "credentials",
    "provider-credentials",
    "iam",
    "network-exposure",
];

/// Labels that take the post-land review route.
const POST_REVIEW_LABELS: [&str; 10] = [
    "authority",
    "agent-authority",
    "issue-authority",
    "custody",
    "lander",
    "queue",
    "merge-queue",
    "rolling-queue",
    "deploy",
    "deployment",
];

/// The starter an eligible Issue runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Route {
    Pipeline,
    PostReview,
}

impl Route {
    pub(crate) const fn starter(self) -> &'static str {
        match self {
            Self::Pipeline => ISSUE_PIPELINE,
            Self::PostReview => ISSUE_PIPELINE_POST_REVIEW,
        }
    }
}

/// Why an Issue is not auto-dispatched, as a stable code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Skip {
    NotOpen,
    Archived,
    NotOptedIn,
    SatelliteClaimed,
    NeedsManagerJudgement(String),
}

impl Skip {
    pub(crate) fn code(&self) -> String {
        match self {
            Self::NotOpen => "not_open".into(),
            Self::Archived => "archived".into(),
            Self::NotOptedIn => "not_opted_in".into(),
            Self::SatelliteClaimed => "satellite_claimed".into(),
            Self::NeedsManagerJudgement(label) => format!("needs_manager_judgement:{label}"),
        }
    }
}

fn normalized(label: &str) -> String {
    label.trim().to_ascii_lowercase()
}

/// Whether the Issue carries the opt-in label.
pub(crate) fn opted_in(labels: &[String]) -> bool {
    labels.iter().any(|label| normalized(label) == AUTO_LABEL)
}

/// The pure eligibility rule for one Issue row.
///
/// # Errors
/// The first [`Skip`] reason that applies.
pub(crate) fn classify(issue: &Issue) -> std::result::Result<Route, Skip> {
    if issue.archived_at.is_some() {
        return Err(Skip::Archived);
    }
    if issue.status != IssueStatus::Open {
        return Err(Skip::NotOpen);
    }
    if !opted_in(&issue.labels) {
        return Err(Skip::NotOptedIn);
    }
    if issue
        .assignee
        .as_deref()
        .is_some_and(|assignee| normalized(assignee).starts_with("satellite:"))
    {
        return Err(Skip::SatelliteClaimed);
    }
    let labels: Vec<String> = issue.labels.iter().map(|label| normalized(label)).collect();
    if let Some(label) = labels
        .iter()
        .find(|label| PRE_MERGE_REVIEW_LABELS.contains(&label.as_str()))
    {
        return Err(Skip::NeedsManagerJudgement(label.clone()));
    }
    if labels
        .iter()
        .any(|label| POST_REVIEW_LABELS.contains(&label.as_str()))
    {
        return Ok(Route::PostReview);
    }
    Ok(Route::Pipeline)
}

/// Concurrent executions the project may have live: the manager's own
/// `max_active_sessions`, never above the build-heavy cap.
pub(crate) fn capacity_limit(max_active_sessions: u16) -> usize {
    usize::from(max_active_sessions).min(BUILD_HEAVY_CAP)
}

type Backoff = Mutex<HashMap<(Uuid, i64), Instant>>;

fn backoff() -> &'static Backoff {
    static BACKOFF: OnceLock<Backoff> = OnceLock::new();
    BACKOFF.get_or_init(|| Mutex::new(HashMap::new()))
}

fn backing_off(project: Uuid, number: i64, now: Instant) -> bool {
    let Ok(mut map) = backoff().lock() else {
        return false;
    };
    map.retain(|_, until| *until > now);
    map.contains_key(&(project, number))
}

pub(crate) fn note_refused(project: Uuid, number: i64, now: Instant) {
    if let Ok(mut map) = backoff().lock() {
        map.insert((project, number), now + REFUSAL_BACKOFF);
    }
}

/// One request the dispatcher will make as `caller`.
#[derive(Clone, Debug)]
pub(crate) struct Dispatch {
    pub(crate) caller: Uuid,
    pub(crate) project: Uuid,
    pub(crate) issue_number: i64,
    pub(crate) route: Route,
    pub(crate) request: AgentTopologyExecuteRequestV1,
}

/// Projects with an `Open` Issue carrying the opt-in label.
///
/// # Errors
/// A store failure.
pub(crate) fn candidate_projects(store: &Store) -> Result<Vec<Uuid>> {
    let mut statement = store.conn.prepare(
        "SELECT DISTINCT i.project_id FROM issues i
         WHERE i.status='Open' AND i.archived_at IS NULL
           AND EXISTS (SELECT 1 FROM json_each(i.labels) WHERE lower(trim(value))=?1)",
    )?;
    let rows = statement.query_map([AUTO_LABEL], |row| row.get::<_, String>(0))?;
    let mut projects = Vec::new();
    for row in rows {
        if let Ok(project) = Uuid::parse_str(&row?) {
            projects.push(project);
        }
    }
    Ok(projects)
}

/// Executions of the project that are not settled, whatever node they wait at.
fn open_executions(store: &Store, project: Uuid) -> Result<usize> {
    let count: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM topology_executions
         WHERE project_id=?1 AND status IN ('accepted','running','cancelling')",
        [project.to_string()],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// Open executions that hold, or are about to hold, a build-heavy session:
/// one with a session node reserved, launching or running, plus one that has
/// no node attempt yet (just accepted). An execution parked at a land node
/// (merge queue wait), in review or on a decision holds no session and is not
/// counted, so a backed-up queue cannot starve new dispatches (hub #3699).
fn session_holding_executions(store: &Store, project: Uuid) -> Result<usize> {
    let count: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM topology_executions e
         WHERE e.project_id=?1 AND e.status IN ('accepted','running','cancelling')
           AND (EXISTS (SELECT 1 FROM topology_node_attempts a
                        WHERE a.execution_id=e.id AND a.node_kind='session'
                          AND a.status IN ('reserved','launching','running'))
                OR NOT EXISTS (SELECT 1 FROM topology_node_attempts a
                               WHERE a.execution_id=e.id))",
        [project.to_string()],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// Any execution at all (any requester, any status) whose input is this Issue.
fn ever_executed(store: &Store, project: Uuid, number: i64) -> Result<bool> {
    Ok(store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM topology_executions
         WHERE json_extract(input_json,'$.issue.project_id')=?1
           AND json_extract(input_json,'$.issue.display_number')=?2)",
        rusqlite::params![project.to_string(), number],
        |row| row.get(0),
    )?)
}

/// The Epic the seat's executions run under: the one it used last when still
/// in scope and executable, else the first in-scope Epic that is.
fn dispatch_epic(store: &Store, caller: &TopologyCaller, project: Uuid) -> Option<Uuid> {
    let TopologyCaller::Manager { authority, .. } = caller else {
        return None;
    };
    let last: Option<Uuid> = store
        .conn
        .query_row(
            "SELECT epic_id FROM topology_executions
             WHERE project_id=?1 AND requested_by_kind='manager' AND epic_id IS NOT NULL
             ORDER BY rowid DESC LIMIT 1",
            [project.to_string()],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|text| Uuid::parse_str(&text).ok());
    last.into_iter()
        .chain(authority.config.epic_ids.iter().copied())
        .find(|epic| {
            authority.config.epic_ids.contains(epic)
                && caller.require_epic(store, *epic, Reach::Effect).is_ok()
        })
}

fn starter_id(name: &str) -> Option<Uuid> {
    starters::STARTERS
        .iter()
        .find(|starter| starter.name == name)
        .map(|starter| starter.id)
}

/// The requests to make for one project right now. Empty when there is no
/// live manager seat that may execute, no spare capacity, or no eligible Issue.
///
/// # Errors
/// A store failure.
pub(crate) fn plan_project(store: &Store, project: Uuid, now: Instant) -> Result<Vec<Dispatch>> {
    let Some(config) = store.get_harness_manager_notice_config(project)? else {
        return Ok(Vec::new());
    };
    let Some(seat) = config.current_session_id.filter(|_| !config.is_revoked()) else {
        return Ok(Vec::new());
    };
    let Ok(caller) = TopologyCaller::resolve(store, seat) else {
        return Ok(Vec::new());
    };
    let TopologyCaller::Manager { authority, .. } = &caller else {
        return Ok(Vec::new());
    };
    let Some(epic_id) = dispatch_epic(store, &caller, project) else {
        return Ok(Vec::new());
    };
    let limit = capacity_limit(authority.grant.policy.max_active_sessions);
    let live = session_holding_executions(store, project)?;
    let open = open_executions(store, project)?;
    let slots = limit
        .saturating_sub(live)
        .min(OPEN_EXECUTIONS_CAP.saturating_sub(open))
        .min(PER_PROJECT_TICK_LIMIT);
    if slots == 0 {
        return Ok(Vec::new());
    }
    let mut plans = Vec::new();
    for issue in store.list_ready_issues(Some(project), None)? {
        if plans.len() >= slots {
            break;
        }
        if !opted_in(&issue.labels) {
            continue;
        }
        let number = issue.display_number;
        let route = match classify(&issue) {
            Ok(route) => route,
            Err(skip) => {
                tracing::debug!(issue = number, reason = %skip.code(), "auto dispatch skipped");
                continue;
            }
        };
        if backing_off(project, number, now)
            || !store
                .live_topology_executions_for_issue(project, number)?
                .is_empty()
            || store.live_issue_worker_holder(issue.id)?.is_some()
            || ever_executed(store, project, number)?
        {
            continue;
        }
        let Some(topology_id) = starter_id(route.starter()) else {
            continue;
        };
        let Some(expected_digest) =
            crate::topology::agent::executable_digest(store, &caller, topology_id, epic_id)
        else {
            tracing::debug!(
                starter = route.starter(),
                "auto dispatch starter unavailable"
            );
            continue;
        };
        plans.push(Dispatch {
            caller: seat,
            project,
            issue_number: number,
            route,
            request: AgentTopologyExecuteRequestV1 {
                topology_id,
                expected_digest,
                epic_id,
                inputs: json!({ starters::ISSUE_INPUT_KEY: number }),
                base_commit: None,
                idempotency_key: format!("{KEY_PREFIX}{project}:{number}"),
                project_id: None,
                on_call: None,
                supersedes_execution: None,
                wake: None,
            },
        });
    }
    Ok(plans)
}

impl SessionManager {
    /// One dispatcher pass (#3390): launch every eligible `dispatch:auto`
    /// Issue that admission and capacity allow. Returns how many executions
    /// were accepted. Cheap when no Issue carries the label (one query).
    ///
    /// # Errors
    /// A store failure while looking for candidates.
    pub async fn dispatch_auto_issues(self: &Arc<Self>) -> Result<usize> {
        let knobs: AgentKnobs = self.topology_agent_knobs();
        if !knobs.executor_enabled {
            return Ok(0);
        }
        let projects = {
            let store =
                crate::store_lock::lock(&self.store, "topology::auto_dispatch::candidates").await;
            candidate_projects(&store)?
        };
        if projects.is_empty() {
            return Ok(0);
        }
        // Host-load and pressure admission: hold the whole pass while the
        // host is busy; the Issues stay Open and are picked up next tick.
        if self.host_load().hold_now().is_some() || self.host_load().pressure_now().is_some() {
            return Ok(0);
        }
        let now = Instant::now();
        let mut accepted = 0;
        for project in projects {
            let plans = {
                let store =
                    crate::store_lock::lock(&self.store, "topology::auto_dispatch::plan").await;
                match plan_project(&store, project, now) {
                    Ok(plans) => plans,
                    Err(error) => {
                        tracing::warn!(%project, %error, "auto dispatch plan deferred");
                        continue;
                    }
                }
            };
            for plan in plans {
                match self
                    .agent_topology_execute(plan.caller, plan.request.clone())
                    .await
                {
                    Ok(result) => {
                        if !result.deduplicated {
                            accepted += 1;
                        }
                        tracing::info!(
                            issue = plan.issue_number,
                            starter = plan.route.starter(),
                            execution = %result.execution_id,
                            "auto dispatch started a topology execution"
                        );
                    }
                    Err(error) => {
                        note_refused(plan.project, plan.issue_number, Instant::now());
                        tracing::warn!(
                            issue = plan.issue_number,
                            starter = plan.route.starter(),
                            %error,
                            "auto dispatch refused; backing off this Issue"
                        );
                    }
                }
            }
        }
        Ok(accepted)
    }
}
