//! #1930, #2109: an Issue-bound worker that fails on provider capacity ("model
//! at capacity", HTTP 503) is recovered by the daemon instead of ending
//! `Failed` for the manager to notice and relaunch by hand.
//!
//! Two stages, both bounded:
//!
//! 1. The transient-failure heal (`store::transient_heal`) resumes the SAME
//!    session on the same model with #123's backoff, [`CAPACITY_RESUME_MAX_ATTEMPTS`]
//!    times. That covers a failure at launch and one hours into the work alike.
//! 2. When the resume bound is spent (or no resume was ever armed), this module
//!    relaunches the Issue binding: an ordinary Issue-bound `create_session`
//!    admission ([`Store::enqueue_issue_worker_inner`]) of the SAME journalled
//!    operation, so the one-live-worker-per-Issue rule, the launch allowlist and
//!    the manager's terminal watch (armed at launch success) all apply
//!    unchanged. The relaunch goes to the next allowed launch of the manager's
//!    policy, other provider family first ([`plan_capacity_retry`]). A worker
//!    that already ran tools is continued (`continue_from`: the new sandbox
//!    branches from the failed worker's committed HEAD and the brief carries its
//!    final message and the Issue's latest handoff); a worker that failed before
//!    its first tool call restarts from the original brief.
//!
//! The substitution is journalled on the action's Issue binding and named in
//! the Issue note ("capacity: relaunched as <id> on <model>"). While a resume or
//! a relaunch is owed the failed worker's manager watch is held, and it is
//! retired when the relaunch is admitted, so the manager is notified only once
//! every attempt failed (the last worker's own watch, annotated by
//! [`Store::capacity_watch_note`]). Auth, invalid-model and quota failures never
//! match [`is_provider_capacity_stop_reason`] and keep failing fast.
//!
//! Why #1930 alone missed the real failures (#2109): it relaunched only a worker
//! with no `ToolUse` event and no `Assistant` message, but Codex records the
//! provider failure itself as two `Assistant` messages ("Provider diagnostic",
//! "Process Error"), so every real capacity failure looked like a started turn
//! and the manager's watch fired `retry-eligible: false`.

use super::{ACTION_KIND, ManagerActionOriginV2};
use crate::error::Result;
use crate::store::Store;
use crate::store::capacity_recovery::BACKOFF_SECONDS;
use crate::store::harness_manager_v2::{ManagerCallerV1, refused};
use crate::store::transient_heal::CAPACITY_RESUME_MAX_ATTEMPTS;
use crate::store_support::provider_defaults::is_provider_capacity_stop_reason;
use chrono::{DateTime, Duration, Utc};
use rsi_common::harness_manager_v2::{ManagerActionV2, ManagerLaunchChoiceV2};
use rsi_common::manager_issue_worker::AgentManagerLaunchIssueWorkerRequestV1;
use rsi_common::types::{CapabilityClass, Session, SessionStatus};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::issue_worker::ManagerIssueWorkerAdmission;

/// Relaunches per Issue-bound launch chain before the manager is notified.
pub const CAPACITY_MAX_RETRIES: u8 = 4;

/// How long past its due time a relaunch keeps the manager watch held. If the
/// daemon cannot admit the relaunch in this window the watch fires normally.
const HOLD_GRACE_SECS: i64 = 300;

/// Failures older than this are never relaunched by the daemon.
const RELAUNCH_WINDOW_HOURS: i64 = 6;

/// The least a failure waits before a relaunch: the transient heal arms its
/// resume in the finalizer right after the row settles `Failed`, and a pending
/// resume owns the failure first.
const HEAL_SETTLE_SECS: i64 = 30;

/// The substitution (or same-launch retry) journalled on the relaunch's
/// Issue binding; visible on the action record and in the Issue note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityRetryV1 {
    /// The failed launch action this one replaces.
    pub predecessor_operation: Uuid,
    /// The failed worker (its manager watch is retired on admission).
    pub predecessor_worker: Uuid,
    /// 1-based relaunch number in the chain.
    pub attempt: u8,
    pub from: ManagerLaunchChoiceV2,
    pub to: ManagerLaunchChoiceV2,
    /// The relaunch continues the failed worker's committed work
    /// (`continue_from` = the failed worker) instead of restarting the brief.
    #[serde(default)]
    pub continued: bool,
}

impl CapacityRetryV1 {
    pub fn substituted(&self) -> bool {
        self.from != self.to
    }

    /// The launch the relaunch uses, as `Provider/model[/effort]`.
    pub fn target_name(&self) -> String {
        launch_name(&self.to)
    }

    pub fn describe(&self) -> String {
        let to = launch_name(&self.to);
        let how = if self.continued {
            "continuing the failed worker's committed work"
        } else {
            "restarting from the original brief"
        };
        if self.substituted() {
            format!(
                "launch substituted {} -> {to}, {how}",
                launch_name(&self.from)
            )
        } else {
            format!("same launch {to} after backoff, {how}")
        }
    }
}

/// `Provider/model[/effort]`, the form every capacity note names a launch in.
fn launch_name(launch: &ManagerLaunchChoiceV2) -> String {
    format!(
        "{:?}/{}{}",
        launch.provider,
        launch.model,
        launch
            .effort
            .as_deref()
            .map(|effort| format!("/{effort}"))
            .unwrap_or_default()
    )
}

/// The next relaunch for a chain: the launch to use and the wait (seconds)
/// after the failure. `tried` lists the launches of the chain so far, oldest
/// first (never empty); `allowed` is the manager's effective launch list.
/// `None` once [`CAPACITY_MAX_RETRIES`] relaunches were made.
///
/// The same model was already resumed [`CAPACITY_RESUME_MAX_ATTEMPTS`] times
/// before a relaunch is planned, so the plan moves on: the next allowed launch
/// not yet tried, ranked (same capability class, other provider family, policy
/// order). Capacity is per selected model, so another model of the same
/// provider is the second choice, not an excluded one. With nothing untried
/// left the same launch repeats on the next backoff step (#123's table).
pub fn plan_capacity_retry(
    tried: &[ManagerLaunchChoiceV2],
    allowed: &[ManagerLaunchChoiceV2],
) -> Option<(ManagerLaunchChoiceV2, i64)> {
    let current = tried.last()?;
    let retries = tried.len() - 1;
    if retries >= usize::from(CAPACITY_MAX_RETRIES) {
        return None;
    }
    let class = CapabilityClass::classify(&current.model);
    let same_model = |a: &ManagerLaunchChoiceV2, b: &ManagerLaunchChoiceV2| {
        a.provider == b.provider && a.model == b.model
    };
    let fallback = allowed
        .iter()
        .enumerate()
        .filter(|(_, launch)| !tried.iter().any(|seen| same_model(seen, launch)))
        .min_by_key(|(order, launch)| {
            (
                class.is_some() && CapabilityClass::classify(&launch.model) != class,
                launch.provider == current.provider,
                *order,
            )
        });
    Some(match fallback {
        Some((_, launch)) => (launch.clone(), 0),
        None => (
            current.clone(),
            BACKOFF_SECONDS[retries.min(BACKOFF_SECONDS.len() - 1)],
        ),
    })
}

/// One failed Issue-bound worker that is owed (or was given) a relaunch.
#[derive(Debug, Clone)]
pub struct CapacityRetryDue {
    pub worker: Uuid,
    pub operation: Uuid,
    pub caller: Uuid,
    /// When the relaunch becomes due (failure time plus the planned wait).
    pub due_at: DateTime<Utc>,
    /// The next launch exists only when attempts remain.
    pub next: Option<(ManagerLaunchChoiceV2, i64)>,
    /// A relaunch of this launch is already admitted.
    pub successor_admitted: bool,
    /// Attempts made before this one (0 for the original launch).
    pub retries_so_far: u8,
    pub project_request_id: Option<Uuid>,
    pub operation_json: ManagerActionV2,
    pub qa_lane: bool,
    pub review_of: Option<i64>,
    /// #218: the failed launch's journalled read scope, kept by the relaunch.
    pub read_scope: super::issue_worker::LaunchReadScope,
    pub continue_from: Option<Uuid>,
    pub issue_display: i64,
    /// The failed worker ran tools, so its committed work is worth continuing
    /// (a failure before the first tool call restarts from the brief).
    pub mid_work: bool,
    /// The brief of the chain's first launch: a continuation re-prefaces this,
    /// never an earlier continuation's already prefaced query.
    pub root_query: String,
    /// While the same-session resume is armed, when it fires. The relaunch
    /// waits for it (and the manager's watch stays held).
    pub resume_pending_until: Option<DateTime<Utc>>,
}

struct StoredLaunch {
    id: Uuid,
    payload: serde_json::Value,
}

impl Store {
    fn stored_launch(&self, operation: Uuid) -> Result<Option<StoredLaunch>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_operations WHERE id=?1 AND kind=?2",
                params![operation.to_string(), ACTION_KIND],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|json| {
            Ok::<_, crate::error::DaemonError>(StoredLaunch {
                id: operation,
                payload: serde_json::from_str(&json)?,
            })
        })
        .transpose()
    }

    /// The launches of the chain ending at `operation`, oldest first, and the
    /// query of the chain's first launch.
    fn capacity_chain_launches(
        &self,
        operation: &StoredLaunch,
        launch: &ManagerLaunchChoiceV2,
    ) -> Result<(Vec<ManagerLaunchChoiceV2>, Option<String>)> {
        let query_of = |payload: &serde_json::Value| {
            payload
                .pointer("/request/operation/query")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        let mut root_query = query_of(&operation.payload);
        let mut chain = vec![launch.clone()];
        let mut cursor = operation
            .payload
            .pointer("/issue_binding/capacity_retry/predecessor_operation")
            .and_then(|v| v.as_str())
            .and_then(|v| Uuid::parse_str(v).ok());
        for _ in 0..=CAPACITY_MAX_RETRIES {
            let Some(id) = cursor else { break };
            let Some(prior) = self.stored_launch(id)? else {
                break;
            };
            if let Some(launch) = prior
                .payload
                .pointer("/request/operation/launch")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
            {
                chain.insert(0, launch);
            }
            root_query = query_of(&prior.payload).or(root_query);
            cursor = prior
                .payload
                .pointer("/issue_binding/capacity_retry/predecessor_operation")
                .and_then(|v| v.as_str())
                .and_then(|v| Uuid::parse_str(v).ok());
        }
        Ok((chain, root_query))
    }

    fn caller_allowed_launches(
        &self,
        caller: Uuid,
        project: Option<Uuid>,
    ) -> Result<Vec<ManagerLaunchChoiceV2>> {
        Ok(match self.resolve_manager_caller(caller, project)? {
            ManagerCallerV1::Legacy {
                config,
                is_manager: true,
            } => {
                self.get_harness_manager_policy(config.project_id)?
                    .ok_or_else(|| refused("manager_v2_grant_required"))?
                    .policy
                    .allowed_launches
            }
            ManagerCallerV1::Global(authority) => authority.grant.policy.allowed_launches,
            ManagerCallerV1::Legacy { .. } | ManagerCallerV1::Area(_) => {
                return Err(refused("manager_v2_scope_denied"));
            }
        })
    }

    /// The relaunch state of `worker` when it is a failed Issue-bound worker
    /// whose launch ended on provider capacity before its first turn.
    /// `Ok(None)` for every other worker and every other failure.
    pub fn issue_worker_capacity_retry_state(
        &self,
        worker: Uuid,
    ) -> Result<Option<CapacityRetryDue>> {
        let found: Option<(String, i64, bool, bool)> = self
            .conn
            .query_row(
                "SELECT o.id, o.rowid,
                        EXISTS(SELECT 1 FROM harness_manager_v2_operations n
                               WHERE n.kind=?1 AND n.rowid>o.rowid
                                 AND json_extract(n.payload_json,'$.issue_binding.issue_id')
                                   =json_extract(o.payload_json,'$.issue_binding.issue_id')),
                        EXISTS(SELECT 1 FROM harness_manager_v2_operations n
                               WHERE n.kind=?1 AND n.rowid>o.rowid
                                 AND json_extract(n.payload_json,
                                       '$.issue_binding.capacity_retry.predecessor_operation')=o.id)
                 FROM harness_manager_v2_operations o
                 WHERE o.kind=?1 AND o.target_session_id=?2 AND o.state='succeeded'
                   AND json_extract(o.payload_json,'$.issue_binding') IS NOT NULL
                 ORDER BY o.rowid DESC LIMIT 1",
                params![ACTION_KIND, worker.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((operation, _rowid, later_launch, successor_admitted)) = found else {
            return Ok(None);
        };
        let Ok(operation) = Uuid::parse_str(&operation) else {
            return Ok(None);
        };
        let Some(session) = self.get_session(worker)? else {
            return Ok(None);
        };
        if session.status != SessionStatus::Failed
            || !session
                .stop_reason
                .as_deref()
                .is_some_and(is_provider_capacity_stop_reason)
        {
            return Ok(None);
        }
        // Mid-work: the worker ran at least one tool, so its committed work is
        // continued. Assistant messages say nothing here: Codex records the
        // provider failure itself as two of them (#2109).
        let mid_work: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_events
              WHERE session_id=?1 AND event_type='ToolUse')",
            [worker.to_string()],
            |row| row.get(0),
        )?;
        let Some(stored) = self.stored_launch(operation)? else {
            return Ok(None);
        };
        let (Ok(origin), Ok(op), Some(binding)) = (
            serde_json::from_value::<ManagerActionOriginV2>(
                stored.payload.get("origin").cloned().unwrap_or_default(),
            ),
            serde_json::from_value::<ManagerActionV2>(
                stored
                    .payload
                    .pointer("/request/operation")
                    .cloned()
                    .unwrap_or_default(),
            ),
            stored.payload.get("issue_binding"),
        ) else {
            return Ok(None);
        };
        let ManagerActionOriginV2::Agent { caller } = origin else {
            return Ok(None);
        };
        let ManagerActionV2::CreateSession { launch, .. } = &op else {
            return Ok(None);
        };
        let (chain, root_query) = self.capacity_chain_launches(&stored, launch)?;
        let retries_so_far = u8::try_from(chain.len() - 1).unwrap_or(u8::MAX);
        let project_request_id = stored
            .payload
            .pointer("/request/project_id")
            .and_then(|v| v.as_str())
            .and_then(|v| Uuid::parse_str(v).ok());
        let next = if later_launch && !successor_admitted {
            // A manager relaunched by hand: nothing is owed.
            None
        } else {
            let allowed = self
                .caller_allowed_launches(caller, project_request_id)
                .unwrap_or_default();
            plan_capacity_retry(&chain, &allowed)
        };
        let wait = next
            .as_ref()
            .map_or(0, |(_, wait)| (*wait).max(HEAL_SETTLE_SECS));
        let ManagerActionV2::CreateSession { query, .. } = &op else {
            return Ok(None);
        };
        Ok(Some(CapacityRetryDue {
            worker,
            operation: stored.id,
            caller,
            due_at: session.updated_at + Duration::seconds(wait),
            mid_work,
            root_query: root_query.unwrap_or_else(|| query.clone()),
            resume_pending_until: self
                .capacity_resume_state(worker)?
                .and_then(|state| state.pending_until),
            next,
            successor_admitted,
            retries_so_far,
            project_request_id,
            operation_json: op,
            qa_lane: binding
                .get("qa_lane")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            review_of: binding.get("review_of").and_then(|v| v.as_i64()),
            read_scope: super::issue_worker::LaunchReadScope {
                related: binding
                    .get("related")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default(),
                brief_refs: binding
                    .get("brief_refs")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default(),
            },
            continue_from: binding
                .get("continue_from")
                .and_then(|v| v.as_str())
                .and_then(|v| Uuid::parse_str(v).ok()),
            issue_display: binding
                .get("display_number")
                .and_then(|v| v.as_i64())
                .unwrap_or_default(),
        }))
    }

    /// Whether the manager's terminal watch on `worker` must stay quiet: the
    /// daemon is still recovering a capacity failure (#1930, #2109). A resume
    /// of the same session is armed and inside its grace window, a relaunch is
    /// admitted, or a relaunch is owed and still inside its grace window.
    pub fn capacity_recovery_holds_watch(&self, worker: Uuid, now: DateTime<Utc>) -> Result<bool> {
        let Some(due) = self.issue_worker_capacity_retry_state(worker)? else {
            return self.capacity_resume_holds_watch(worker, now);
        };
        if due.successor_admitted {
            return Ok(true);
        }
        if let Some(until) = due.resume_pending_until {
            return Ok(now <= until + Duration::seconds(HOLD_GRACE_SECS));
        }
        Ok(due.next.is_some() && now <= due.due_at + Duration::seconds(HOLD_GRACE_SECS))
    }

    /// A session (bound or not) whose capacity failure has a same-session
    /// resume armed that has not yet had its grace window run out.
    fn capacity_resume_holds_watch(&self, session: Uuid, now: DateTime<Utc>) -> Result<bool> {
        let Some(row) = self.get_session(session)? else {
            return Ok(false);
        };
        if row.status != SessionStatus::Failed
            || !row
                .stop_reason
                .as_deref()
                .is_some_and(is_provider_capacity_stop_reason)
        {
            return Ok(false);
        }
        Ok(self
            .capacity_resume_state(session)?
            .and_then(|state| state.pending_until)
            .is_some_and(|until| now <= until + Duration::seconds(HOLD_GRACE_SECS)))
    }

    /// Failed Issue-bound workers whose relaunch is due at `now`, oldest
    /// launch first. Bounded; the caller admits each with
    /// [`Self::enqueue_issue_worker_capacity_retry`].
    /// The latest (at most 64, newest launch first) Issue-bound launches whose
    /// worker failed on provider capacity in the relaunch window and whose Issue
    /// has no later launch.
    ///
    /// #2464: the manager-action pass runs this every few seconds under the
    /// store lock, so it starts from the few recently failed sessions (the
    /// terminal `updated_at` partial index) and checks supersession only for
    /// their launches. The previous single query evaluated the "no later launch
    /// of this Issue" subquery for every succeeded launch before joining the
    /// session filter: a JSON parse of every later operation per operation,
    /// 1.4 s per pass on the hub's 4,400 launches, which was the persistence
    /// worker's 100 ms-1.4 s store lock waits.
    fn capacity_failed_issue_workers(&self, now: DateTime<Utc>) -> Result<Vec<String>> {
        let since = (now - Duration::hours(RELAUNCH_WINDOW_HOURS))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let failed = {
            // The redundant terminal-status list matches the partial index
            // `idx_sessions_terminal_reclaim_page_v119(updated_at,id)` exactly,
            // so SQLite range-scans recent terminal sessions only.
            let mut statement = self.conn.prepare_cached(
                "SELECT id FROM sessions
                 WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                   AND updated_at > ?1 AND status='Failed'
                   AND stop_reason LIKE 'provider_error:%'
                   AND (lower(stop_reason) LIKE '%at capacity%' OR stop_reason LIKE '%503%'
                        OR lower(stop_reason) LIKE '%overloaded%'
                        OR lower(stop_reason) LIKE '%service unavailable%')",
            )?;
            statement
                .query_map([&since], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut launches: Vec<(i64, String, rusqlite::types::Value)> = Vec::new();
        for worker in failed {
            let mut statement = self.conn.prepare_cached(
                "SELECT o.rowid, json_extract(o.payload_json,'$.issue_binding.issue_id')
                 FROM harness_manager_v2_operations o
                 WHERE o.kind=?1 AND o.state='succeeded' AND o.target_session_id=?2
                   AND json_extract(o.payload_json,'$.issue_binding') IS NOT NULL",
            )?;
            let rows = statement
                .query_map(params![ACTION_KIND, worker], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<(i64, rusqlite::types::Value)>>>()?;
            launches.extend(
                rows.into_iter()
                    .map(|(rowid, issue)| (rowid, worker.clone(), issue)),
            );
        }
        launches.sort_unstable_by_key(|launch| std::cmp::Reverse(launch.0));
        let mut workers = Vec::new();
        for (rowid, worker, issue) in launches {
            if workers.len() == 64 {
                break;
            }
            // A launch without an Issue id is never superseded (SQL NULL never
            // equals a later launch's id), exactly as the single query read it.
            let superseded = !matches!(issue, rusqlite::types::Value::Null)
                && self
                    .conn
                    .prepare_cached(
                        "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations n
                      WHERE n.rowid>?2 AND n.kind=?1
                        AND json_extract(n.payload_json,'$.issue_binding.issue_id')=?3)",
                    )?
                    .query_row(params![ACTION_KIND, rowid, issue], |row| {
                        row.get::<_, bool>(0)
                    })?;
            if !superseded {
                workers.push(worker);
            }
        }
        Ok(workers)
    }

    pub fn list_issue_worker_capacity_retries_due(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<CapacityRetryDue>> {
        let workers = self.capacity_failed_issue_workers(now)?;
        let mut due = Vec::new();
        for worker in workers {
            let Ok(worker) = Uuid::parse_str(&worker) else {
                continue;
            };
            if let Some(state) = self.issue_worker_capacity_retry_state(worker)?
                && !state.successor_admitted
                && state.resume_pending_until.is_none()
                && state.next.is_some()
                && state.due_at <= now
            {
                due.push(state);
            }
        }
        Ok(due)
    }

    /// Admit the relaunch of one due worker (idempotent on
    /// `capacity-retry:<operation>:<attempt>`). `Ok(None)` when nothing is
    /// owed any more (the Issue moved on, attempts are exhausted).
    pub fn enqueue_issue_worker_capacity_retry(
        &self,
        due: &CapacityRetryDue,
    ) -> Result<Option<ManagerIssueWorkerAdmission>> {
        let Some((to, _)) = due.next.clone() else {
            return Ok(None);
        };
        let ManagerActionV2::CreateSession {
            parent_id,
            kind,
            query,
            launch: from,
            sandbox_source,
        } = due.operation_json.clone()
        else {
            return Ok(None);
        };
        let attempt = due.retries_so_far + 1;
        let continued = due.mid_work;
        let meta = CapacityRetryV1 {
            predecessor_operation: due.operation,
            predecessor_worker: due.worker,
            attempt,
            from: from.clone(),
            to: to.clone(),
            continued,
        };
        // A continuation re-prefaces the chain's first brief and takes its
        // sandbox from the failed worker (`enqueue_issue_worker_inner`); a
        // restart reuses the failed launch's query and sandbox verbatim.
        let operation = ManagerActionV2::CreateSession {
            parent_id,
            kind,
            query: if continued {
                due.root_query.clone()
            } else {
                query
            },
            launch: to.clone(),
            sandbox_source,
        };
        let request = AgentManagerLaunchIssueWorkerRequestV1 {
            project_id: due.project_request_id,
            issue: due.issue_display,
            brief: "provider capacity relaunch".into(),
            launch: to,
            parent_epic_id: Some(parent_id),
            idempotency_key: format!("capacity-retry:{}:{attempt}", due.operation),
            sandbox_source: None,
            continue_from: if continued {
                Some(due.worker)
            } else {
                due.continue_from
            },
            qa_lane: due.qa_lane,
            review_of: due.review_of,
            related: Vec::new(),
        };
        let admission = self.enqueue_issue_worker_inner(
            due.caller,
            &request,
            Some((operation, meta)),
            Some(due.read_scope.clone()),
            &[],
        )?;
        Ok(Some(admission))
    }

    /// Disable every armed capacity resume of a worker the manager already
    /// replaced by hand (a later launch of the same Issue exists): resuming
    /// the old worker would run two sessions on one Issue. Returns the count.
    pub fn retire_superseded_capacity_resumes(&self, now: DateTime<Utc>) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE scheduled_jobs SET enabled=0, updated_at=?1
             WHERE enabled=1 AND wake_mode='resume' AND json_valid(schedule_json)
               AND json_extract(schedule_json,'$.transient_heal.last_reason')=?3
               AND EXISTS(SELECT 1 FROM harness_manager_v2_operations o
                          WHERE o.kind=?2 AND o.state='succeeded'
                            AND o.target_session_id=scheduled_jobs.wake_session_id
                            AND json_extract(o.payload_json,'$.issue_binding') IS NOT NULL
                            AND EXISTS(SELECT 1 FROM harness_manager_v2_operations n
                                       WHERE n.kind=?2 AND n.rowid>o.rowid
                                         AND json_extract(n.payload_json,'$.issue_binding.issue_id')
                                           =json_extract(o.payload_json,'$.issue_binding.issue_id')))",
            params![
                now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ACTION_KIND,
                crate::model_control::transient_heal::PROVIDER_CAPACITY_HEAL_REASON
            ],
        )?)
    }

    /// The terminal-watch annotation for a session that met a provider
    /// capacity failure (#2109), so the manager reads what the daemon did
    /// instead of a bare `Failed`:
    /// - `capacity: relaunched as <id> on <model> (from <id>, attempt n/4)` on
    ///   the worker the daemon launched in place of a failed one;
    /// - `capacity: relaunched as <id> on <model>` / `capacity: resumed (n/3)` /
    ///   `capacity: recovery exhausted (...)` on a failed worker;
    /// - `capacity: resumed n/3` on a worker the heal resumed that went on.
    pub fn capacity_watch_note(&self, session: &Session) -> Result<Option<String>> {
        let short = |id: Uuid| id.to_string()[..8].to_string();
        let retry_meta: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.issue_binding.capacity_retry')
                 FROM harness_manager_v2_operations
                 WHERE kind=?1 AND target_session_id=?2 AND state='succeeded'
                 ORDER BY rowid DESC LIMIT 1",
                params![ACTION_KIND, session.id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let mut notes = Vec::new();
        if let Some(meta) =
            retry_meta.and_then(|json| serde_json::from_str::<CapacityRetryV1>(&json).ok())
        {
            notes.push(format!(
                "capacity: relaunched as {} on {} (from {}, attempt {}/{})",
                short(session.id),
                launch_name(&meta.to),
                short(meta.predecessor_worker),
                meta.attempt,
                CAPACITY_MAX_RETRIES
            ));
        }
        let failed_on_capacity = session.status == SessionStatus::Failed
            && session
                .stop_reason
                .as_deref()
                .is_some_and(is_provider_capacity_stop_reason);
        let resume = self.capacity_resume_state(session.id)?;
        if failed_on_capacity {
            let successor = self.capacity_successor(session.id)?;
            match (&successor, &resume) {
                (Some((worker, to)), _) => notes.push(format!(
                    "capacity: relaunched as {} on {}",
                    short(*worker),
                    launch_name(to)
                )),
                (None, Some(state)) if state.pending_until.is_some() => notes.push(format!(
                    "capacity: resumed ({}/{})",
                    state.attempts, state.max_attempts
                )),
                (None, state) => notes.push(format!(
                    "capacity: recovery exhausted (resumed {}/{} on the same model, no relaunch admitted)",
                    state.as_ref().map_or(0, |state| state.attempts),
                    CAPACITY_RESUME_MAX_ATTEMPTS
                )),
            }
        } else if let Some(state) = resume.filter(|state| state.attempts > 0) {
            notes.push(format!(
                "capacity: resumed {}/{}",
                state.attempts, state.max_attempts
            ));
        }
        Ok((!notes.is_empty()).then(|| notes.join(", ")))
    }

    /// The worker the daemon launched in place of a failed one, and its launch.
    fn capacity_successor(&self, failed: Uuid) -> Result<Option<(Uuid, ManagerLaunchChoiceV2)>> {
        let found: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT n.target_session_id, json_extract(n.payload_json,'$.issue_binding.capacity_retry.to')
                 FROM harness_manager_v2_operations n
                 WHERE n.kind=?1
                   AND json_extract(n.payload_json,'$.issue_binding.capacity_retry.predecessor_worker')=?2
                 ORDER BY n.rowid DESC LIMIT 1",
                params![ACTION_KIND, failed.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(found.and_then(|(worker, to)| {
            Some((
                Uuid::parse_str(&worker).ok()?,
                serde_json::from_str(&to).ok()?,
            ))
        }))
    }

    /// Retire the manager's terminal watch on a failed worker whose relaunch
    /// was just admitted, inside the admission transaction.
    pub(super) fn retire_capacity_predecessor_watch(
        conn: &rusqlite::Connection,
        predecessor: Uuid,
        manager: Uuid,
    ) -> Result<()> {
        let mut statement = conn.prepare(
            "SELECT id FROM scheduled_jobs
             WHERE enabled=1 AND wake_mode=?1 AND wake_session_id=?2",
        )?;
        let ids = statement
            .query_map(
                params![format!("on_terminal:{predecessor}"), manager.to_string()],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let ids: Vec<Uuid> = ids
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect();
        crate::store::scheduled_jobs::disable_scheduled_jobs_conn(conn, &ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::SessionProvider;

    fn launch(model: &str) -> ManagerLaunchChoiceV2 {
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Codex,
            model: model.into(),
            effort: Some("xhigh".into()),
        }
    }

    fn claude(model: &str) -> ManagerLaunchChoiceV2 {
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: model.into(),
            effort: None,
        }
    }

    /// #2109: the same model was already resumed in place, so the first
    /// relaunch moves on: the other provider family, with no wait.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn first_relaunch_moves_to_the_other_provider_family_without_waiting() {
        let sol = launch("gpt-6-sol");
        let opus = claude("claude-opus-5");
        let allowed = [sol.clone(), launch("gpt-6-astra"), opus.clone()];
        let plan = plan_capacity_retry(std::slice::from_ref(&sol), &allowed).expect("owed");
        assert_eq!(plan, (opus, 0));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn same_capability_class_outranks_the_other_family() {
        let sol = launch("gpt-6-sol");
        let astra = launch("gpt-6-astra");
        let allowed = [
            sol.clone(),
            launch("gpt-6-luna"),
            claude("claude-haiku-5"),
            astra.clone(),
        ];
        let plan = plan_capacity_retry(&[sol], &allowed).expect("owed");
        assert_eq!(plan, (astra, 0));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_tried_model_is_never_chosen_again_even_with_another_effort() {
        let sol = launch("gpt-6-sol");
        let mut sol_high = sol.clone();
        sol_high.effort = Some("high".into());
        let opus = claude("claude-opus-5");
        let allowed = [sol.clone(), sol_high, opus.clone()];
        assert_eq!(
            plan_capacity_retry(std::slice::from_ref(&sol), &allowed),
            Some((opus.clone(), 0))
        );
        // After the other family also failed, nothing untried is left: the
        // chain repeats its current launch on the next backoff step.
        assert_eq!(
            plan_capacity_retry(&[sol, opus.clone()], &allowed),
            Some((opus, BACKOFF_SECONDS[1]))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn without_an_alternative_the_same_launch_backs_off_further_until_exhausted() {
        let sol = launch("gpt-6-sol");
        let allowed = [sol.clone()];
        let one = plan_capacity_retry(std::slice::from_ref(&sol), &allowed).expect("owed");
        assert_eq!(one, (sol.clone(), BACKOFF_SECONDS[0]));
        let two = plan_capacity_retry(&[sol.clone(), sol.clone()], &allowed).expect("owed");
        assert_eq!(two, (sol.clone(), BACKOFF_SECONDS[1]));
        let exhausted = vec![sol.clone(); usize::from(CAPACITY_MAX_RETRIES) + 1];
        assert_eq!(plan_capacity_retry(&exhausted, &allowed), None);
    }

    /// #2464: `background` Issue launches of finished workers plus two workers
    /// that failed on capacity, one of them already replaced by a later launch
    /// of its Issue. Returns the failed workers the scan reports, the expected
    /// one, and the SQLite VM instructions the scan took.
    fn capacity_scan(background: usize) -> (Vec<String>, String, u64) {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::types::{Project, SessionKind};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let project = Uuid::new_v4();
        store
            .insert_project(&Project {
                id: project,
                name: format!("capacity scan {project}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let session = |status: SessionStatus, kind: SessionKind, stop: Option<&str>| {
            let id = Uuid::new_v4();
            let mut row = crate::test_support::test_session(id, "/tmp/capacity-scan".into());
            row.project_id = Some(project);
            row.status = status;
            row.session_kind = kind;
            row.stop_reason = stop.map(str::to_string);
            store.insert_session(&row).unwrap();
            id
        };
        let manager = session(SessionStatus::Completed, SessionKind::Standard, None);
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: manager,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        let launch = |worker: Uuid, issue: Uuid| {
            store
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                        scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                        state,target_session_id,outcome_json,not_before,created_at,updated_at)
                     VALUES(?1,?2,?3,1,1,?1,'fixture',?4,?5,'succeeded',?6,'{}',?7,?7,?7)",
                    params![
                        Uuid::new_v4().to_string(),
                        project.to_string(),
                        manager.to_string(),
                        ACTION_KIND,
                        serde_json::json!({
                            "request": {"operation": {"action": "create_session"}},
                            "issue_binding": {"issue_id": issue.to_string(), "padding": "x".repeat(512)}
                        })
                        .to_string(),
                        worker.to_string(),
                        now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                    ],
                )
                .unwrap();
        };
        let capacity = "provider_error:codex:Selected model is at capacity.";
        let replaced_issue = Uuid::new_v4();
        let replaced = session(SessionStatus::Failed, SessionKind::Task, Some(capacity));
        launch(replaced, replaced_issue);
        for _ in 0..background {
            let done = session(SessionStatus::Completed, SessionKind::Task, None);
            launch(done, Uuid::new_v4());
        }
        let owed = session(SessionStatus::Failed, SessionKind::Task, Some(capacity));
        launch(owed, Uuid::new_v4());
        // The manager relaunched the replaced worker's Issue by hand.
        let successor = session(SessionStatus::Running, SessionKind::Task, None);
        launch(successor, replaced_issue);

        let counter = Arc::new(AtomicU64::new(0));
        let handle = Arc::clone(&counter);
        store.conn.progress_handler(
            1,
            Some(move || {
                handle.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let workers = store.capacity_failed_issue_workers(now).unwrap();
        store.conn.progress_handler(1, None::<fn() -> bool>);
        (workers, owed.to_string(), counter.load(Ordering::Relaxed))
    }

    /// #2464: the capacity relaunch scan runs on every manager-action pass
    /// under the store lock. It reports exactly the failed worker whose Issue
    /// has no later launch, and its cost is linear in the launch history (the
    /// old form parsed every later launch's JSON for every launch: quadratic,
    /// 1.4 s per pass on the hub).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn capacity_relaunch_scan_is_linear_in_launch_history_2464() {
        let (small_workers, small_owed, small) = capacity_scan(100);
        let (large_workers, large_owed, large) = capacity_scan(400);
        assert_eq!(small_workers, vec![small_owed]);
        assert_eq!(large_workers, vec![large_owed]);
        assert!(
            large < small * 6,
            "scan cost grew {large} / {small} for 4x the launch history: not linear"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn only_transient_capacity_text_is_classified_as_capacity() {
        for transient in [
            "provider_error:codex:Selected model is at capacity. Please try a different model.",
            "provider_error:codex:Reconnecting... 5/5 (503)",
            "provider_error:codex:Service Unavailable",
            "provider_error:codex:model overloaded",
        ] {
            assert!(is_provider_capacity_stop_reason(transient), "{transient}");
        }
        for fatal in [
            "provider_error:codex_usage_limit",
            "provider_error:provider_auth_invalid",
            "provider_error:codex:invalid model",
            "provider_error:codex:unauthorized (401)",
            "provider_error:codex:exceeded your quota",
            "provider_error:unclassified",
            "daemon_restart_resume:at capacity",
        ] {
            assert!(!is_provider_capacity_stop_reason(fatal), "{fatal}");
        }
    }
}
