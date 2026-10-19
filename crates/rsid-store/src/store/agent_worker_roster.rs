//! `AgentWorkerRoster` (#2311): the store half. One read-only snapshot that
//! joins a worker's session row, its daemon jobs, its armed wakes and its
//! latest turn into the state a manager acts on.
//!
//! No state is stored here and no schema is added. Every derived fact comes
//! from a predicate the daemon already owns, so the roster cannot disagree with
//! the stranded notice (#1555) or the one-worker-per-Issue launch check
//! (#2212):
//! - `final_handoff`: [`Store::latest_turn_final_result`] (#3090: a marker beside a
//!   job-result wake the daemon still owes is provisional, so the worker is parked);
//! - `parked_on_job`: [`Store::owner_running_job_has_completion_wake`];
//! - `parked_on_wake`: [`Store::owner_armed_result_wake`], the one liveness
//!   predicate of a parked worker (it counts `resume`, `when` and `on_terminal`
//!   wakes the worker owns);
//! - `stranded`: a `Completed` turn with none of the above.

use super::Store;
use super::manager_actions::ACTION_KIND;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_jobs::{JobKind, JobState};
use rsi_common::agent_worker_roster::{
    AgentJobSummaryV1, AgentWorkerRosterRequestV1, AgentWorkerRosterResultV1,
    ROSTER_CONFLICT_PATHS_MAX, RosterQueuedLaunchV1, RosterWakeV1, RosterWorkerV1,
    WORKER_ROSTER_CALLER_UNAVAILABLE, WORKER_ROSTER_NOT_AUTHORIZED, WORKER_ROSTER_SCAN_CAP,
    WORKER_ROSTER_STALE_STRANDED_DAYS, WorkerRosterScopeKindV1, WorkerRosterScopeV1,
    WorkerRosterStateV1,
};
use rsi_common::types::{Session, SessionStatus};
use rsi_common::wake_predicate::WakeWhenState;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Most wakes listed per worker (the daemon caps `when` wakes at 16 per session).
const ROSTER_WAKES_PER_WORKER: i64 = 16;

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_id(raw: &str, what: &str) -> Result<Uuid> {
    Uuid::parse_str(raw).map_err(|error| DaemonError::Store(format!("invalid {what} id: {error}")))
}

impl Store {
    /// The worker roster of `caller`'s scope. See [`AgentWorkerRosterRequestV1`].
    ///
    /// # Errors
    /// `worker_roster_invalid_request`, `worker_roster_caller_unavailable`,
    /// `worker_roster_not_authorized` (a `project_id` the caller does not
    /// manage), `manager_project_not_in_scope` (a portfolio seat naming a
    /// project outside its grant), or a persistence error.
    pub fn agent_worker_roster(
        &self,
        caller: Uuid,
        request: &AgentWorkerRosterRequestV1,
    ) -> Result<AgentWorkerRosterResultV1> {
        self.agent_worker_roster_at(caller, request, Utc::now())
    }

    /// [`Self::agent_worker_roster`] with the clock named, so the stale-strand
    /// cutoff is testable.
    pub(crate) fn agent_worker_roster_at(
        &self,
        caller: Uuid,
        request: &AgentWorkerRosterRequestV1,
        now: DateTime<Utc>,
    ) -> Result<AgentWorkerRosterResultV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        // One consistent snapshot for the whole derivation.
        let _snapshot = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let session = self
            .get_session(caller)?
            .filter(|session| {
                rsi_common::is_leaf_kind(session.session_kind)
                    && !matches!(
                        session.status,
                        SessionStatus::Archived | SessionStatus::Deleted
                    )
            })
            .ok_or_else(|| DaemonError::InvalidParam(WORKER_ROSTER_CALLER_UNAVAILABLE.into()))?;
        let project = self.worker_roster_project(&session, request.project_id)?;
        let candidates = match project {
            Some(project) => {
                self.roster_project_candidates(project, caller, None, WORKER_ROSTER_SCAN_CAP + 1)?
            }
            None => self.roster_own_candidates(caller, None, WORKER_ROSTER_SCAN_CAP + 1)?,
        };
        let mut truncated = candidates.len() > WORKER_ROSTER_SCAN_CAP;
        let page = request.page_limit();
        let mut queued_launches = self.roster_queued_launches(caller, project, page + 1, now)?;
        truncated |= queued_launches.len() > page;
        queued_launches.truncate(page);
        let stale_before = now - chrono::Duration::days(WORKER_ROSTER_STALE_STRANDED_DAYS);
        let mut stale_stranded_hidden = 0u32;
        let mut workers = Vec::new();
        for id in candidates.into_iter().take(WORKER_ROSTER_SCAN_CAP) {
            let Some(worker_session) = self.get_session(id)? else {
                continue;
            };
            let Some(worker) = self.roster_worker(&worker_session)? else {
                continue;
            };
            if !request.states.is_empty() && !request.states.contains(&worker.state) {
                continue;
            }
            // #2362: old strands are history; the default view drops them.
            if request.states.is_empty()
                && worker.state == WorkerRosterStateV1::Stranded
                && worker_session.updated_at < stale_before
            {
                stale_stranded_hidden += 1;
                continue;
            }
            if workers.len() == page {
                truncated = true;
                break;
            }
            workers.push(worker);
        }
        let mut counts: BTreeMap<WorkerRosterStateV1, u32> = BTreeMap::new();
        for worker in &workers {
            *counts.entry(worker.state).or_default() += 1;
        }
        Ok(AgentWorkerRosterResultV1 {
            scope: WorkerRosterScopeV1 {
                kind: if project.is_some() {
                    WorkerRosterScopeKindV1::Project
                } else {
                    WorkerRosterScopeKindV1::OwnLaunched
                },
                project_id: project,
            },
            count: u32::try_from(workers.len()).unwrap_or(u32::MAX),
            workers,
            queued_launches,
            counts,
            truncated,
            stale_stranded_hidden,
        })
    }

    fn roster_queued_launches(
        &self,
        caller: Uuid,
        project: Option<Uuid>,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<RosterQueuedLaunchV1>> {
        let mut statement = self.conn.prepare(
            "SELECT o.id,o.target_session_id,o.not_before
               FROM harness_manager_v2_operations o
              WHERE o.kind=?1 AND o.state='queued' AND o.not_before<=?2
                AND json_extract(o.payload_json,'$.request.operation.action')='create_session'
                AND json_extract(o.payload_json,'$.issue_binding.continue_from') IS NULL
                AND ((?3 IS NOT NULL AND o.project_id=?3)
                     OR (?3 IS NULL AND o.actor_session_id=?4))
              ORDER BY o.not_before,o.id LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                ACTION_KIND,
                stamp(now),
                project.map(|id| id.to_string()),
                caller.to_string(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        rows.map(|row| {
            let (id, target, not_before) = row?;
            Ok(RosterQueuedLaunchV1 {
                operation_id: parse_id(&id, "operation")?,
                target_session_id: target.map(|id| parse_id(&id, "target")).transpose()?,
                not_before,
                held_reason: None,
            })
        })
        .collect()
    }

    /// Whether `target` is a live worker the caller's `AgentWorkerRoster` scope
    /// covers right now (#2418), so `AgentListWakes {session_id}` serves exactly
    /// the sessions the roster would list. It reuses the roster's scope
    /// resolution ([`Self::worker_roster_project`]) and candidate queries,
    /// narrowed to `target`, so there is one authority rule: the caller's
    /// default scope (its project as the appointed manager, else the workers it
    /// launched itself), plus the target's project when a portfolio seat's grant
    /// covers it. Live state every call: a replaced manager, a narrowed grant or
    /// an archived worker is not covered.
    ///
    /// # Errors
    /// `worker_roster_caller_unavailable` or a persistence error. An unknown or
    /// uncovered target is `Ok(false)`.
    pub fn roster_authorizes_session(&self, caller: Uuid, target: Uuid) -> Result<bool> {
        if caller == target {
            return Ok(false);
        }
        let _snapshot = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let session = self
            .get_session(caller)?
            .filter(|session| {
                rsi_common::is_leaf_kind(session.session_kind)
                    && !matches!(
                        session.status,
                        SessionStatus::Archived | SessionStatus::Deleted
                    )
            })
            .ok_or_else(|| DaemonError::InvalidParam(WORKER_ROSTER_CALLER_UNAVAILABLE.into()))?;
        let Some(target_session) = self.get_session(target)? else {
            return Ok(false);
        };
        let default_scope = self.worker_roster_project(&session, None)?;
        let covered = match default_scope {
            Some(project) => !self
                .roster_project_candidates(project, caller, Some(target), 1)?
                .is_empty(),
            None => !self
                .roster_own_candidates(caller, Some(target), 1)?
                .is_empty(),
        };
        if covered {
            return Ok(true);
        }
        // A portfolio seat (or a manager of another project) also reads the
        // target's project when `AgentWorkerRoster {project_id}` would serve it.
        let Some(project) = target_session
            .project_id
            .filter(|p| Some(*p) != default_scope)
        else {
            return Ok(false);
        };
        match self.worker_roster_project(&session, Some(project)) {
            Ok(Some(project)) => Ok(!self
                .roster_project_candidates(project, caller, Some(target), 1)?
                .is_empty()),
            Ok(None) | Err(DaemonError::InvalidParam(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// `AgentListWakes` (#2389, #2418): the wakes `caller` may read. `target`
    /// `None` or the caller's own id is the caller-bound self read; any other
    /// id is read only when [`Self::roster_authorizes_session`] holds, else
    /// `list_wakes_target_not_authorized` (an unknown id is refused the same
    /// way). Read-only.
    ///
    /// # Errors
    /// `list_wakes_target_not_authorized`, `worker_roster_caller_unavailable`
    /// or a persistence error.
    pub fn list_wake_views_for_reader(
        &self,
        caller: Uuid,
        target: Option<Uuid>,
        include_disabled: bool,
        limit: usize,
    ) -> Result<Vec<rsi_common::agent_wake_list::AgentWakeV1>> {
        let owner = target.unwrap_or(caller);
        if owner != caller && !self.roster_authorizes_session(caller, owner)? {
            return Err(DaemonError::InvalidParam(
                rsi_common::agent_wake_list::LIST_WAKES_TARGET_NOT_AUTHORIZED.into(),
            ));
        }
        self.list_owned_wake_views(owner, include_disabled, limit)
    }

    /// The project whose workers the caller may list, or `None` for the
    /// workers the caller launched itself. A project is served to its current
    /// appointed manager (the committed lineage tip, so a successor sees its
    /// predecessor's workers) and to a portfolio seat whose grant covers it;
    /// both are reads, so no capability is needed beyond holding the seat.
    fn worker_roster_project(
        &self,
        caller: &Session,
        requested: Option<Uuid>,
    ) -> Result<Option<Uuid>> {
        let is_project_manager = |project: Uuid| -> Result<bool> {
            Ok(caller.project_id == Some(project)
                && self
                    .get_harness_manager(project)?
                    .and_then(|config| config.current_session_id)
                    == Some(caller.id))
        };
        match requested {
            None => match caller.project_id {
                Some(project) if is_project_manager(project)? => Ok(Some(project)),
                _ => Ok(None),
            },
            Some(project) => {
                if is_project_manager(project)? {
                    return Ok(Some(project));
                }
                // `manager_project_not_in_scope` for a seat outside its grant.
                match self.global_project_principal(caller.id, project)? {
                    Some(_) => Ok(Some(project)),
                    None => Err(DaemonError::InvalidParam(
                        WORKER_ROSTER_NOT_AUTHORIZED.into(),
                    )),
                }
            }
        }
    }

    /// Workers the caller launched itself, live, newest activity first: its
    /// `AgentSpawnChild` children, the node sessions of topology executions it
    /// requested (#2914) and (#2362) the sessions its own
    /// `AgentManagerLaunchIssueWorker` actions created, whose parent is the
    /// launch action rather than the caller. The launcher is the operation's
    /// `actor_session_id`.
    fn roster_own_candidates(
        &self,
        caller: Uuid,
        only: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT s.id FROM sessions s
              WHERE s.id<>?1 AND s.status NOT IN ('Archived','Deleted')
                AND s.session_kind NOT IN ('Group','Epic')
                AND (?4 IS NULL OR s.id=?4)
                AND (s.id IN (
                       SELECT r.child_session_id FROM agent_spawn_requests r
                        WHERE r.owner_session_id=?1 AND r.state='launched')
                  OR s.id IN (
                       SELECT o.target_session_id FROM harness_manager_v2_operations o
                        WHERE o.actor_session_id=?1 AND o.kind=?2
                          AND o.state IN ('running','succeeded')
                          AND o.target_session_id IS NOT NULL
                          AND json_extract(o.payload_json,'$.request.operation.action')
                              ='create_session')
                  OR s.id IN (
                       SELECT a.session_id FROM topology_node_attempts a
                         JOIN topology_executions e ON e.id=a.execution_id
                        WHERE e.requested_by_session_id=?1
                          AND a.session_id IS NOT NULL))
              ORDER BY s.updated_at DESC, s.id LIMIT ?3",
        )?;
        let rows = statement
            .query_map(
                params![
                    caller.to_string(),
                    ACTION_KIND,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    only.map(|id| id.to_string())
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter().map(|id| parse_id(id, "worker")).collect()
    }

    /// Workers launched in `project`: sessions a manager created
    /// (`create_session`, with or without an Issue binding), children the
    /// project's Epic leads spawned and (#2914) the node sessions of the
    /// project's topology executions, live, newest activity first. The manager
    /// itself is never listed.
    fn roster_project_candidates(
        &self,
        project: Uuid,
        caller: Uuid,
        only: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT s.id FROM sessions s
              WHERE s.id<>?2
                AND s.status NOT IN ('Archived','Deleted')
                AND s.session_kind NOT IN ('Group','Epic')
                AND (?5 IS NULL OR s.id=?5)
                AND ((s.project_id=?1 AND (s.id IN (
                       SELECT o.target_session_id FROM harness_manager_v2_operations o
                        WHERE o.project_id=?1 AND o.kind=?3
                          AND o.state IN ('running','succeeded')
                          AND o.target_session_id IS NOT NULL
                          AND json_extract(o.payload_json,'$.request.operation.action')
                              ='create_session')
                  OR s.id IN (
                       SELECT r.child_session_id FROM agent_spawn_requests r
                         JOIN sessions p ON p.id=r.owner_session_id
                        WHERE r.state='launched' AND p.project_id=?1)))
                  OR s.id IN (
                       SELECT a.session_id FROM topology_node_attempts a
                         JOIN topology_executions e ON e.id=a.execution_id
                        WHERE e.project_id=?1 AND a.session_id IS NOT NULL))
              ORDER BY s.updated_at DESC, s.id LIMIT ?4",
        )?;
        let rows = statement
            .query_map(
                params![
                    project.to_string(),
                    caller.to_string(),
                    ACTION_KIND,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    only.map(|id| id.to_string())
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter().map(|id| parse_id(id, "worker")).collect()
    }

    /// One worker's roster row; `None` for a session that is not a live worker
    /// (archived or deleted).
    fn roster_worker(&self, session: &Session) -> Result<Option<RosterWorkerV1>> {
        let id = session.id;
        // Only a turn that has ended can carry a final report; a mid-turn
        // session reports false rather than a half-written turn.
        // #3090: a marker written while the worker's job-result wake is still
        // owed is provisional (#2054); the worker is parked, not final.
        let mut reported = match session.status {
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted => {
                self.latest_turn_final_result(id)?
            }
            _ => false,
        };
        let state = match session.status {
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval => {
                WorkerRosterStateV1::Running
            }
            SessionStatus::Failed | SessionStatus::Interrupted => WorkerRosterStateV1::Failed,
            SessionStatus::Completed => {
                if reported {
                    WorkerRosterStateV1::FinalHandoff
                } else if self.owner_running_job_has_completion_wake(id)? {
                    WorkerRosterStateV1::ParkedOnJob
                } else if self.owner_armed_result_wake(id)?.is_some() {
                    WorkerRosterStateV1::ParkedOnWake
                } else if self.worker_published_issue_handoff(id)? {
                    // #2362: the verdict is on the Issue; only the marker is
                    // missing from the turn.
                    reported = true;
                    WorkerRosterStateV1::FinalHandoff
                } else {
                    WorkerRosterStateV1::Stranded
                }
            }
            _ => return Ok(None),
        };
        let open_jobs: i64 = self.conn.query_row(
            "SELECT count(*) FROM agent_jobs
              WHERE owner_session_id=?1 AND state IN ('queued','running')",
            params![id.to_string()],
            |row| row.get(0),
        )?;
        Ok(Some(RosterWorkerV1 {
            session_id: id,
            issue: Self::bound_issue_display_for_worker_on(&self.conn, id)?,
            provider: format!("{:?}", session.provider),
            model: session.model.clone(),
            effort: session.effort.clone(),
            session_status: format!("{:?}", session.status),
            state,
            latest_job: self.roster_latest_job(id)?,
            open_jobs: u32::try_from(open_jobs).unwrap_or(u32::MAX),
            pending_wakes: self.roster_pending_wakes(id)?,
            last_activity_at: stamp(session.updated_at),
            final_handoff: reported,
        }))
    }

    /// Whether `worker` published an Issue handoff (`AgentUpdateIssue
    /// {handoff}`, the append-only `issue_handoffs` record). A completed worker
    /// with nothing to resume it that did so has handed off, whether or not
    /// its turn carries the `PIPELINE HANDOFF` marker (#2362).
    fn worker_published_issue_handoff(&self, worker: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM issue_handoffs WHERE author_session_id=?1)",
            params![worker.to_string()],
            |row| row.get(0),
        )?)
    }

    /// The worker's newest daemon job, minimal. `receipt_head` is copied from
    /// the result's receipt, never derived or checked here.
    fn roster_latest_job(&self, owner: Uuid) -> Result<Option<AgentJobSummaryV1>> {
        let row: Option<(String, String, String, Option<i64>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, kind, state, exit_code, result_json FROM agent_jobs
                  WHERE owner_session_id=?1 ORDER BY sequence DESC LIMIT 1",
                params![owner.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, kind, state, exit_code, result_json)) = row else {
            return Ok(None);
        };
        let result: Option<serde_json::Value> = result_json
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok());
        let text = |pointer: &str| {
            result
                .as_ref()
                .and_then(|value| value.pointer(pointer))
                .and_then(serde_json::Value::as_str)
                .map(|text| text.chars().take(200).collect::<String>())
        };
        Ok(Some(AgentJobSummaryV1 {
            job_id: parse_id(&id, "job")?,
            kind: JobKind::parse(&kind)
                .ok_or_else(|| DaemonError::Store(format!("invalid job kind {kind}")))?,
            state: JobState::parse(&state)
                .ok_or_else(|| DaemonError::Store(format!("invalid job state {state}")))?,
            exit_code: exit_code.and_then(|code| i32::try_from(code).ok()),
            refusal: text("/refusal"),
            receipt_head: text("/receipt/head"),
            merges_cleanly_onto_rolling: result
                .as_ref()
                .and_then(|value| value.pointer("/receipt/merges_cleanly_onto_rolling"))
                .and_then(serde_json::Value::as_bool),
            conflict_paths: result
                .as_ref()
                .and_then(|value| value.pointer("/receipt/conflict_paths"))
                .and_then(serde_json::Value::as_array)
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .take(ROSTER_CONFLICT_PATHS_MAX)
                        .map(|path| path.chars().take(200).collect())
                        .collect()
                })
                .unwrap_or_default(),
        }))
    }

    /// Enabled wakes the worker owns that resume it: the same set as
    /// [`Store::owner_armed_result_wake`], bounded.
    fn roster_pending_wakes(&self, owner: Uuid) -> Result<Vec<RosterWakeV1>> {
        let mut statement = self.conn.prepare(
            "SELECT id, wake_mode, next_fire_at, schedule_json FROM scheduled_jobs
              WHERE wake_session_id=?1 AND enabled=1
                AND (wake_mode='resume'
                  OR json_type(schedule_json,'$.wake_when') IS NOT NULL
                  OR wake_mode LIKE 'on_terminal:%')
              ORDER BY created_at, id LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![owner.to_string(), ROSTER_WAKES_PER_WORKER], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, mode, next_fire_at, schedule_json)| {
                let (mode, watch_session_id) = match mode.strip_prefix("on_terminal:") {
                    Some(watched) => ("on_terminal", Uuid::parse_str(watched).ok()),
                    None if mode == "resume" => ("resume", None),
                    None => ("fresh", None),
                };
                let when: Option<WakeWhenState> =
                    serde_json::from_str::<serde_json::Value>(&schedule_json)
                        .ok()
                        .and_then(|schedule| schedule.get("wake_when").cloned())
                        .and_then(|state| serde_json::from_value(state).ok());
                Ok(RosterWakeV1 {
                    wake_id: parse_id(&id, "wake")?,
                    mode: mode.to_string(),
                    watch_session_id,
                    deadline: when.as_ref().and_then(|state| state.deadline).map(stamp),
                    when: when.map(|state| state.predicate),
                    next_fire_at,
                })
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "agent_worker_roster_tests.rs"]
mod tests;
