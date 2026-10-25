//! `AgentWorkerRoster` (#2311): one read that tells a manager what each of its
//! workers is really doing.
//!
//! A session's `Completed` status is shown both for a worker parked on its own
//! running job and for one that wrote its final handoff. The roster derives the
//! state a manager acts on from the same predicates the daemon already uses
//! for the stranded notice and the one-worker-per-Issue check (a final
//! `RESULT`/`PIPELINE HANDOFF` in the latest turn, an armed `resume`/`when`/
//! watch wake of the worker's own, a daemon job it owns), so the roster and
//! those notices never disagree.
//!
//! The request names no session: the scope is the caller's own launched
//! workers, or one project's workers for that project's appointed manager and
//! for a portfolio seat whose grant covers it. The daemon resolves both from
//! the transport token.

use crate::agent_jobs::{JobKind, JobState};
use crate::wake_predicate::WakePredicate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// A malformed request (bad `limit`, nil `project_id`, duplicate `states`).
pub const WORKER_ROSTER_INVALID_REQUEST: &str = "worker_roster_invalid_request";
/// `project_id` names a project the caller does not manage (it is neither the
/// project's appointed manager nor a portfolio seat over it).
pub const WORKER_ROSTER_NOT_AUTHORIZED: &str = "worker_roster_not_authorized";
/// The caller is not a live leaf session.
pub const WORKER_ROSTER_CALLER_UNAVAILABLE: &str = "worker_roster_caller_unavailable";

/// Default and maximum page size.
pub const WORKER_ROSTER_DEFAULT_LIMIT: u32 = 64;
pub const WORKER_ROSTER_MAX_LIMIT: u32 = 256;
/// Most candidate workers one call derives, newest activity first. Deriving a
/// completed worker's state reads its latest turn, so the scan is bounded;
/// `truncated` says when older workers were not looked at.
pub const WORKER_ROSTER_SCAN_CAP: usize = 512;
/// A `stranded` worker idle for longer than this many days is left out of the
/// default view (no `states` filter or `fields: full`, #2362): it is history,
/// and it buries the strands that need action. Naming `stranded` in `states` lists
/// every one, as does `fields: full`; the result's `stale_stranded_hidden`
/// counts the omitted rows.
pub const WORKER_ROSTER_STALE_STRANDED_DAYS: i64 = 7;

/// What a worker is doing, derived. The first four are the live states a
/// manager waits on; `stranded` is the one that needs action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRosterStateV1 {
    /// Mid-turn: `Starting`, `Running` or `WaitingApproval`.
    Running,
    /// Turn ended with no final report and a daemon-evaluated `when` wake that
    /// names a job the worker owns that has not settled: the daemon resumes the
    /// worker with that job's result.
    ParkedOnJob,
    /// Turn ended with no final report and another armed wake of the worker's
    /// own (a timer `resume`, a `when` wake on a commit, Issue or to-do, or an
    /// `on_terminal` watch on a child): the daemon resumes the worker.
    ParkedOnWake,
    /// Turn ended with no final report and nothing will resume it. A job it
    /// still owns (`open_jobs`) cannot resume it.
    Stranded,
    /// The latest turn carries a final `RESULT`/`PIPELINE HANDOFF`, or (#2362)
    /// the worker published a handoff on an Issue (`AgentUpdateIssue
    /// {handoff}`) and nothing is left to resume it: a reviewer that wrote its
    /// verdict to the review Issue but omitted the marker is done. A submitted
    /// DB review receipt or membership in a settled topology execution also
    /// finishes a completed worker with no wake obligation.
    FinalHandoff,
    /// `Failed` or `Interrupted`.
    Failed,
}

impl WorkerRosterStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::ParkedOnJob => "parked_on_job",
            Self::ParkedOnWake => "parked_on_wake",
            Self::Stranded => "stranded",
            Self::FinalHandoff => "final_handoff",
            Self::Failed => "failed",
        }
    }
}

/// Strict request. The caller is the transport-bound session; nothing here
/// can name another caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWorkerRosterRequestV1 {
    /// `summary` (default) omits finished workers idle for over an hour and
    /// stale strands. `full` or nonempty `states` restores matching history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<WorkerRosterFieldsV1>,
    /// The project whose workers to list: its appointed manager, or a
    /// portfolio seat whose grant covers it. Omitted, a project's appointed
    /// manager lists its own project's workers and any other caller lists the
    /// workers it launched itself.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// Only workers in these derived states, including old history. Empty
    /// uses the summary view unless `fields` is `full`.
    #[serde(default)]
    pub states: Vec<WorkerRosterStateV1>,
    /// Page size, 1..=256; default 64. Workers are newest activity first.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRosterFieldsV1 {
    Summary,
    Full,
}

impl AgentWorkerRosterRequestV1 {
    #[must_use]
    pub fn full_history(&self) -> bool {
        self.fields == Some(WorkerRosterFieldsV1::Full) || !self.states.is_empty()
    }

    /// # Errors
    /// [`WORKER_ROSTER_INVALID_REQUEST`].
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut seen = std::collections::BTreeSet::new();
        if self.project_id.is_some_and(|id| id.is_nil())
            || self
                .limit
                .is_some_and(|limit| !(1..=WORKER_ROSTER_MAX_LIMIT).contains(&limit))
            || self.states.iter().any(|state| !seen.insert(*state))
        {
            return Err(WORKER_ROSTER_INVALID_REQUEST);
        }
        Ok(())
    }

    #[must_use]
    pub fn page_limit(&self) -> usize {
        self.limit.unwrap_or(WORKER_ROSTER_DEFAULT_LIMIT) as usize
    }
}

/// Which workers the roster covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRosterScopeKindV1 {
    /// Workers the caller launched itself: its `AgentSpawnChild` children and
    /// the sessions its `AgentManagerLaunchIssueWorker` actions created (#2362).
    OwnLaunched,
    /// Every worker a manager launched in the project.
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRosterScopeV1 {
    pub kind: WorkerRosterScopeKindV1,
    /// The project of a `project` scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

/// One daemon job, minimal. The fields are the ones any reader of a job needs
/// to tell what it did; `receipt_head` is the commit a receipt-bearing result
/// (`scoped_test`) was produced for, copied from the result, never derived or
/// checked here. A reader that must decide whether a receipt covers a candidate
/// reuses this struct and compares `receipt_head` itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentJobSummaryV1 {
    pub job_id: Uuid,
    pub kind: JobKind,
    pub state: JobState,
    pub exit_code: Option<i32>,
    /// The result's stable refusal code (`job_timed_out`, `job_cancelled`, ...).
    pub refusal: Option<String>,
    /// `result.receipt.head`, when the result carries one.
    pub receipt_head: Option<String>,
    /// `result.receipt.failure_class`: signal-only compile/test interruptions (#516).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    /// `result.receipt.merges_cleanly_onto_rolling` (#348): whether the
    /// receipt's head merged onto `origin/rolling` when the job ended. Absent
    /// for a receipt that predates the field or when git could not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merges_cleanly_onto_rolling: Option<bool>,
    /// `result.receipt.conflict_paths` when it is false, at most
    /// [`ROSTER_CONFLICT_PATHS_MAX`] of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_paths: Vec<String>,
    /// #582: the live capacity hold of a running job (disk floor, free bytes,
    /// floor, reclaimable candidates), overlaid by the daemon from the
    /// governor; the store never fills it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<crate::agent_jobs::JobHoldV1>,
}

/// At most this many conflicted paths ride on a roster job summary.
pub const ROSTER_CONFLICT_PATHS_MAX: usize = 8;

/// One armed wake the worker owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterWakeV1 {
    pub wake_id: Uuid,
    /// `resume`, `fresh` or `on_terminal`.
    pub mode: String,
    /// The watched session of an `on_terminal` wake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_session_id: Option<Uuid>,
    /// The `when` predicate of a daemon-evaluated wake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<WakePredicate>,
    /// When a `when` wake fires with `timed_out` if still pending (RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    /// The next time the daemon looks at this wake (RFC3339); a polled `when`
    /// wake is re-evaluated every few seconds, so this is not the fire time of
    /// a predicate wake.
    pub next_fire_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterWorkerV1 {
    pub session_id: Uuid,
    /// The Issue display number the worker was launched on, if any.
    pub issue: Option<u32>,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The raw session status (`Running`, `Completed`, ...).
    pub session_status: String,
    pub state: WorkerRosterStateV1,
    /// The worker's newest daemon job.
    pub latest_job: Option<AgentJobSummaryV1>,
    /// Jobs the worker owns that are `queued` or `running`.
    pub open_jobs: u32,
    /// Enabled wakes the worker owns (`resume`, `when`, `on_terminal`).
    pub pending_wakes: Vec<RosterWakeV1>,
    /// `sessions.updated_at` (RFC3339).
    pub last_activity_at: String,
    /// True when the worker's final handoff is on record: the latest turn
    /// carries a final `RESULT`/`PIPELINE HANDOFF` (not a provisional one while a
    /// job-result wake is still owed, #3090), or it published an Issue
    /// handoff, submitted a DB review receipt or belongs to a settled topology
    /// execution and nothing will resume it (`state` is then `final_handoff`).
    pub final_handoff: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterQueuedLaunchV1 {
    pub operation_id: Uuid,
    pub target_session_id: Option<Uuid>,
    /// Earliest launch time (RFC3339); queued launches are oldest first.
    pub not_before: String,
    /// The live host-admission hold, e.g. `disk_pressure` or
    /// `sandbox_free_space`. Absent when host admission permits the launch.
    pub held_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWorkerRosterResultV1 {
    pub scope: WorkerRosterScopeV1,
    pub workers: Vec<RosterWorkerV1>,
    /// Due new creates in the same authorized scope, including launches whose
    /// session does not exist yet. Bounded by the request's page limit;
    /// `count` and `counts` continue to describe existing workers only.
    #[serde(default)]
    pub queued_launches: Vec<RosterQueuedLaunchV1>,
    pub count: u32,
    /// Workers returned, per derived state.
    pub counts: BTreeMap<WorkerRosterStateV1, u32>,
    /// More workers matched, or were left unexamined, than this page holds.
    pub truncated: bool,
    /// `stranded` workers idle for over [`WORKER_ROSTER_STALE_STRANDED_DAYS`]
    /// that the summary view left out; ask for `stranded` explicitly or
    /// `fields: full` to list them. Counts only candidates examined before
    /// the page or scan bound stopped the read.
    #[serde(default)]
    pub stale_stranded_hidden: u32,
    /// `final_handoff`/`failed` workers idle for over an hour omitted by the
    /// summary view, among candidates examined before the page or scan bound.
    /// These are excluded from `count` and `counts`, which describe the page.
    #[serde(default)]
    pub hidden_finished: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_strict_and_bounded() {
        let ok: AgentWorkerRosterRequestV1 = serde_json::from_value(serde_json::json!({
            "states": ["stranded", "parked_on_job"], "limit": 10
        }))
        .unwrap();
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(ok.page_limit(), 10);
        assert_eq!(
            AgentWorkerRosterRequestV1::default().page_limit(),
            WORKER_ROSTER_DEFAULT_LIMIT as usize
        );
        for bad in [
            serde_json::json!({"limit": 0}),
            serde_json::json!({"limit": 257}),
            serde_json::json!({"states": ["stranded", "stranded"]}),
            serde_json::json!({"project_id": Uuid::nil()}),
        ] {
            let request: AgentWorkerRosterRequestV1 = serde_json::from_value(bad).unwrap();
            assert_eq!(request.validate(), Err(WORKER_ROSTER_INVALID_REQUEST));
        }
        // No field can name another caller.
        assert!(
            serde_json::from_value::<AgentWorkerRosterRequestV1>(
                serde_json::json!({"session_id": Uuid::nil()})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<AgentWorkerRosterRequestV1>(
                serde_json::json!({"states": ["asleep"]})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<AgentWorkerRosterRequestV1>(
                serde_json::json!({"fields": "verbose"})
            )
            .is_err()
        );
    }

    #[test]
    fn fields_and_states_select_summary_or_full_history() {
        for (value, full) in [
            (serde_json::json!({}), false),
            (serde_json::json!({"fields": null}), false),
            (
                serde_json::json!({"fields": "summary", "states": []}),
                false,
            ),
            (serde_json::json!({"fields": "full"}), true),
            (serde_json::json!({"states": ["final_handoff"]}), true),
            (
                serde_json::json!({"fields": "summary", "states": ["failed"]}),
                true,
            ),
        ] {
            let request: AgentWorkerRosterRequestV1 =
                serde_json::from_value(value.clone()).unwrap();
            assert_eq!(request.validate(), Ok(()));
            assert_eq!(request.full_history(), full, "{value}");
            let roundtrip: AgentWorkerRosterRequestV1 =
                serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
            assert_eq!(roundtrip, request);
        }
    }

    #[test]
    fn states_serialize_as_the_names_a_manager_filters_on() {
        for state in [
            WorkerRosterStateV1::Running,
            WorkerRosterStateV1::ParkedOnJob,
            WorkerRosterStateV1::ParkedOnWake,
            WorkerRosterStateV1::Stranded,
            WorkerRosterStateV1::FinalHandoff,
            WorkerRosterStateV1::Failed,
        ] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
    }
}
