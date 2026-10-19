//! Tests for the `AgentWorkerRoster` store read (#2311): one per derived
//! state, the scope and authority arms, filtering and paging.

use super::*;
use crate::test_support::test_session;
use chrono::Utc;
use rsi_common::agent_jobs::{JobKind, JobState};
use rsi_common::agent_worker_roster::{
    WORKER_ROSTER_INVALID_REQUEST, WORKER_ROSTER_NOT_AUTHORIZED,
};
use rsi_common::global_manager::{ConfigureGlobalManagerRequestV1, MANAGER_PROJECT_NOT_IN_SCOPE};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::types::{
    ConversationEvent, EventType, NewIssue, Project, Recurrence, Role, ScheduleSpec, ScheduledJob,
    SessionProvider, WakeMode,
};
use rsi_common::wake_predicate::WakePredicate;
use std::path::PathBuf;

struct World {
    store: Store,
    project: Uuid,
    manager: Uuid,
    next_event: std::cell::Cell<i32>,
}

fn project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    store
        .insert_project(&Project {
            id,
            name: format!("{name} {id}"),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    id
}

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

impl World {
    /// A project with an appointed manager.
    fn new() -> Self {
        let store = Store::open_in_memory().unwrap();
        let project = project(&store, "roster");
        let manager = Self::seat(&store, Some(project), SessionStatus::Completed);
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: manager,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        Self {
            store,
            project,
            manager,
            next_event: std::cell::Cell::new(1),
        }
    }

    fn seat(store: &Store, project: Option<Uuid>, status: SessionStatus) -> Uuid {
        let id = Uuid::new_v4();
        let mut row = test_session(id, PathBuf::from("/tmp/roster"));
        row.project_id = project;
        row.status = status;
        row.session_kind = rsi_common::types::SessionKind::Standard;
        store.insert_session(&row).unwrap();
        id
    }

    /// A manager-launched worker (`create_session` operation), optionally on
    /// an Issue, in `status`, last active at `activity`.
    fn worker(&self, status: SessionStatus, issue: Option<Uuid>, activity: &str) -> Uuid {
        let id = Uuid::new_v4();
        let mut row = test_session(id, PathBuf::from("/tmp/roster-worker"));
        row.project_id = Some(self.project);
        row.status = status;
        row.provider = SessionProvider::Claude;
        row.model = Some("claude-sonnet-5-5".into());
        row.effort = Some("high".into());
        self.store.insert_session(&row).unwrap();
        self.store
            .conn
            .execute(
                "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                params![id.to_string(), activity],
            )
            .unwrap();
        let mut payload = serde_json::json!({
            "request": {"operation": {"action": "create_session"}}
        });
        if let Some(issue) = issue {
            payload["issue_binding"] = serde_json::json!({"issue_id": issue.to_string()});
        }
        self.store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                    scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                    state,target_session_id,outcome_json,not_before,created_at,updated_at)
                 VALUES(?1,?2,?3,1,1,?1,'fixture',?4,?5,'succeeded',?6,'{}',?7,?7,?7)",
                params![
                    Uuid::new_v4().to_string(),
                    self.project.to_string(),
                    self.manager.to_string(),
                    ACTION_KIND,
                    payload.to_string(),
                    id.to_string(),
                    activity
                ],
            )
            .unwrap();
        id
    }

    fn issue(&self) -> Uuid {
        self.store
            .create_issue(&NewIssue {
                project_id: self.project,
                title: "roster Issue".into(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap()
            .id
    }

    fn say(&self, session: Uuid, role: Role, content: &str) {
        let sequence = self.next_event.get();
        self.next_event.set(sequence + 1);
        self.store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: session,
                sequence,
                event_type: EventType::Message,
                role: Some(role),
                content: content.into(),
                tool_name: None,
                tool_input: None,
                created_at: Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
    }

    fn job(
        &self,
        owner: Uuid,
        kind: &str,
        state: &str,
        exit_code: Option<i64>,
        result: Option<serde_json::Value>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        self.store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id,owner_session_id,kind,params_json,cwd,unit_name,
                    log_path,status_path,state,exit_code,result_json,created_at,row_version)
                 VALUES(?1,?2,?3,'{}','/tmp',?4,'/tmp/l','/tmp/s',?5,?6,?7,
                    '2026-10-01T00:00:00.000000000Z',1)",
                params![
                    id.to_string(),
                    owner.to_string(),
                    kind,
                    format!("unit-{id}"),
                    state,
                    exit_code,
                    result.map(|value| value.to_string())
                ],
            )
            .unwrap();
        id
    }

    fn wake(&self, owner: Uuid, mode: WakeMode, when: Option<serde_json::Value>) -> Uuid {
        let now = Utc::now();
        let id = Uuid::new_v4();
        self.store
            .insert_scheduled_job(&ScheduledJob {
                id,
                name: format!("wake-{id}"),
                message: "m".into(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: now,
                },
                last_fired_at: None,
                next_fire_at: now,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: mode,
                wake_session_id: Some(owner),
            })
            .unwrap();
        if let Some(when) = when {
            self.store
                .conn
                .execute(
                    "UPDATE scheduled_jobs
                        SET schedule_json=json_set(schedule_json,'$.wake_when',json(?2))
                      WHERE id=?1",
                    params![id.to_string(), when.to_string()],
                )
                .unwrap();
        }
        id
    }

    fn roster(
        &self,
        caller: Uuid,
        request: AgentWorkerRosterRequestV1,
    ) -> Result<AgentWorkerRosterResultV1> {
        self.store.agent_worker_roster_at(caller, &request, now())
    }

    fn manager_roster(&self) -> AgentWorkerRosterResultV1 {
        self.roster(self.manager, AgentWorkerRosterRequestV1::default())
            .unwrap()
    }
}

fn state_of(roster: &AgentWorkerRosterResultV1, worker: Uuid) -> &RosterWorkerV1 {
    roster
        .workers
        .iter()
        .find(|row| row.session_id == worker)
        .unwrap_or_else(|| panic!("worker {worker} missing from the roster"))
}

const T0: &str = "2026-10-13T10:00:00.000000000Z";
const T1: &str = "2026-10-13T11:00:00.000000000Z";
const T2: &str = "2026-10-13T12:00:00.000000000Z";
const T3: &str = "2026-10-13T13:00:00.000000000Z";

/// The clock every roster read in these tests uses, after `T3`.
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-13T14:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn job_wake_predicate(job: Uuid) -> serde_json::Value {
    serde_json::json!({
        "predicate": {"jobs_terminal": [job.to_string()]},
        "armed_at": "2026-10-13T09:00:00Z",
        "deadline": "2026-10-13T10:00:00Z",
        "repo_dir": null
    })
}

/// A worker mid-turn is `running`; its Issue, model, effort and activity show.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_worker_mid_turn_is_running_with_its_issue_model_and_activity() {
    let world = World::new();
    let issue = world.issue();
    let running = world.worker(SessionStatus::Running, Some(issue), T1);
    let waiting = world.worker(SessionStatus::WaitingApproval, None, T0);
    let roster = world.manager_roster();
    assert_eq!(roster.scope.kind, WorkerRosterScopeKindV1::Project);
    assert_eq!(roster.scope.project_id, Some(world.project));

    let row = state_of(&roster, running);
    assert_eq!(row.state, WorkerRosterStateV1::Running);
    assert_eq!(row.issue, Some(1));
    assert_eq!(row.provider, "Claude");
    assert_eq!(row.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(row.effort.as_deref(), Some("high"));
    assert_eq!(row.session_status, "Running");
    assert_eq!(row.last_activity_at, T1);
    assert!(!row.final_handoff);
    assert!(row.latest_job.is_none() && row.pending_wakes.is_empty());
    assert_eq!(row.open_jobs, 0);

    let row = state_of(&roster, waiting);
    assert_eq!(row.state, WorkerRosterStateV1::Running);
    assert_eq!(row.issue, None);
    // Newest activity first.
    assert_eq!(
        roster
            .workers
            .iter()
            .map(|w| w.session_id)
            .collect::<Vec<_>>(),
        [running, waiting]
    );
    assert_eq!(roster.counts.get(&WorkerRosterStateV1::Running), Some(&2));
}

/// A `Completed` worker with a running job and a `when.jobs_terminal` wake on
/// it is `parked_on_job`; its job and wake are shown.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_completed_worker_with_a_job_wake_is_parked_on_the_job() {
    let world = World::new();
    let worker = world.worker(SessionStatus::Completed, None, T1);
    world.say(worker, Role::Assistant, "submitted scoped_test, waiting");
    let job = world.job(worker, "test", "running", None, None);
    let wake = world.wake(worker, WakeMode::Resume, Some(job_wake_predicate(job)));

    let roster = world.manager_roster();
    let row = state_of(&roster, worker);
    assert_eq!(row.state, WorkerRosterStateV1::ParkedOnJob);
    assert_eq!(row.session_status, "Completed");
    assert!(!row.final_handoff);
    assert_eq!(row.open_jobs, 1);
    let latest = row.latest_job.as_ref().unwrap();
    assert_eq!(latest.job_id, job);
    assert_eq!(latest.kind, JobKind::Test);
    assert_eq!(latest.state, JobState::Running);
    assert_eq!(latest.exit_code, None);
    assert_eq!(latest.refusal, None);
    assert_eq!(latest.receipt_head, None);
    assert_eq!(row.pending_wakes.len(), 1);
    let pending = &row.pending_wakes[0];
    assert_eq!(pending.wake_id, wake);
    assert_eq!(pending.mode, "resume");
    assert_eq!(
        pending.when,
        Some(WakePredicate {
            jobs_terminal: Some(vec![job]),
            ..WakePredicate::default()
        })
    );
    assert_eq!(
        pending.deadline.as_deref(),
        Some("2026-10-13T10:00:00.000000000Z")
    );
    assert_eq!(
        roster.counts.get(&WorkerRosterStateV1::ParkedOnJob),
        Some(&1)
    );
}

/// A `Completed` worker with another armed wake (a timer, a commit predicate,
/// or a watch on a child) is `parked_on_wake`, and the wake says what it
/// waits for.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_completed_worker_with_another_armed_wake_is_parked_on_the_wake() {
    let world = World::new();
    let timer = world.worker(SessionStatus::Completed, None, T0);
    world.say(timer, Role::Assistant, "checking back later");
    world.wake(timer, WakeMode::Resume, None);

    let landing = world.worker(SessionStatus::Completed, None, T1);
    world.say(landing, Role::Assistant, "waiting for my commit to land");
    let sha = "a".repeat(40);
    world.wake(
        landing,
        WakeMode::Resume,
        Some(serde_json::json!({
            "predicate": {"sha_on_rolling": sha},
            "armed_at": "2026-10-13T09:00:00Z",
            "deadline": null,
            "repo_dir": "/tmp/r"
        })),
    );

    let watcher = world.worker(SessionStatus::Completed, None, T2);
    world.say(watcher, Role::Assistant, "watching my child");
    let child = Uuid::new_v4();
    world.wake(watcher, WakeMode::OnTerminal(child), None);

    // A job it owns that no wake names is not what parks it.
    let jobless_wake = world.worker(SessionStatus::Completed, None, T3);
    world.say(jobless_wake, Role::Assistant, "tests done earlier");
    world.job(jobless_wake, "test", "succeeded", Some(0), None);
    world.wake(jobless_wake, WakeMode::Resume, None);

    let roster = world.manager_roster();
    for worker in [timer, landing, watcher, jobless_wake] {
        assert_eq!(
            state_of(&roster, worker).state,
            WorkerRosterStateV1::ParkedOnWake,
            "{worker}"
        );
    }
    let timer_wake = &state_of(&roster, timer).pending_wakes[0];
    assert_eq!(
        (timer_wake.mode.as_str(), &timer_wake.when),
        ("resume", &None)
    );
    assert_eq!(
        state_of(&roster, landing).pending_wakes[0].when,
        Some(WakePredicate {
            sha_on_rolling: Some(sha),
            ..WakePredicate::default()
        })
    );
    let watch = &state_of(&roster, watcher).pending_wakes[0];
    assert_eq!(watch.mode, "on_terminal");
    assert_eq!(watch.watch_session_id, Some(child));
    assert_eq!(
        roster.counts.get(&WorkerRosterStateV1::ParkedOnWake),
        Some(&4)
    );
}

/// A `Completed` worker with no final report and nothing that will resume it is
/// `stranded`, even when it left a job running (a running unit cannot resume
/// it). A disabled wake does not count.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_completed_worker_nothing_will_resume_is_stranded() {
    let world = World::new();
    let bare = world.worker(SessionStatus::Completed, None, T0);
    world.say(bare, Role::Assistant, "I will run the tests");

    let owns_job = world.worker(SessionStatus::Completed, None, T1);
    world.say(owns_job, Role::Assistant, "tests are running");
    let job = world.job(owns_job, "test", "running", None, None);

    let retired_wake = world.worker(SessionStatus::Completed, None, T2);
    world.say(retired_wake, Role::Assistant, "waiting");
    let wake = world.wake(retired_wake, WakeMode::Resume, None);
    world
        .store
        .conn
        .execute(
            "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
            [wake.to_string()],
        )
        .unwrap();

    // A watch the manager holds on the worker is not the worker's own wake.
    let watched = world.worker(SessionStatus::Completed, None, T3);
    world.say(watched, Role::Assistant, "waiting");
    world.wake(world.manager, WakeMode::OnTerminal(watched), None);

    let roster = world.manager_roster();
    for worker in [bare, owns_job, retired_wake, watched] {
        let row = state_of(&roster, worker);
        assert_eq!(row.state, WorkerRosterStateV1::Stranded, "{worker}");
        assert!(row.pending_wakes.is_empty() && !row.final_handoff);
    }
    let row = state_of(&roster, owns_job);
    assert_eq!(row.open_jobs, 1);
    assert_eq!(row.latest_job.as_ref().unwrap().job_id, job);
    assert_eq!(roster.counts.get(&WorkerRosterStateV1::Stranded), Some(&4));
}

/// A final `PIPELINE HANDOFF` in the latest turn is `final_handoff`, and the
/// worker's latest job carries its exit code and receipt head.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_roster_job_carries_the_receipts_rolling_merge_fields_348() {
    let world = World::new();
    let worker = world.worker(SessionStatus::Completed, None, T1);
    let paths: Vec<String> = (0..12).map(|index| format!("crates/f{index}.rs")).collect();
    world.job(
        worker,
        "test",
        "succeeded",
        Some(0),
        Some(serde_json::json!({
            "exit_code": 0, "refusal": null, "failing_tests": [],
            "receipt": {
                "head": "c".repeat(40), "ok": true,
                "merges_cleanly_onto_rolling": false, "conflict_paths": paths
            }
        })),
    );
    let roster = world.manager_roster();
    let latest = state_of(&roster, worker).latest_job.as_ref().unwrap();
    assert_eq!(latest.merges_cleanly_onto_rolling, Some(false));
    assert_eq!(latest.conflict_paths.len(), ROSTER_CONFLICT_PATHS_MAX);
    assert_eq!(latest.conflict_paths[0], "crates/f0.rs");

    // A receipt that predates the fields reads as absent.
    let older = world.worker(SessionStatus::Completed, None, T1);
    world.job(
        older,
        "test",
        "succeeded",
        Some(0),
        Some(serde_json::json!({
            "exit_code": 0, "refusal": null, "failing_tests": [],
            "receipt": {"head": "d".repeat(40), "ok": true}
        })),
    );
    let roster = world.manager_roster();
    let latest = state_of(&roster, older).latest_job.as_ref().unwrap();
    assert_eq!(latest.merges_cleanly_onto_rolling, None);
    assert!(latest.conflict_paths.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_completed_worker_with_a_final_handoff_is_final_with_its_receipt_head() {
    let world = World::new();
    let worker = world.worker(SessionStatus::Completed, None, T1);
    let head = "b".repeat(40);
    world.job(
        worker,
        "test",
        "succeeded",
        Some(0),
        Some(serde_json::json!({
            "exit_code": 0, "refusal": null, "failing_tests": [],
            "receipt": {"head": head, "base": "origin/rolling", "ok": true}
        })),
    );
    world.say(
        worker,
        Role::Assistant,
        "PIPELINE HANDOFF — IMPLEMENTATION:\nFriction: none",
    );

    let roster = world.manager_roster();
    let row = state_of(&roster, worker);
    assert_eq!(row.state, WorkerRosterStateV1::FinalHandoff);
    assert!(row.final_handoff);
    let latest = row.latest_job.as_ref().unwrap();
    assert_eq!(latest.state, JobState::Succeeded);
    assert_eq!(latest.exit_code, Some(0));
    assert_eq!(latest.receipt_head.as_deref(), Some(head.as_str()));
    assert_eq!(row.open_jobs, 0);

    // A later user turn without a new report makes it a live worker again.
    world.say(worker, Role::User, "one more thing");
    world.say(worker, Role::Assistant, "on it");
    let roster = world.manager_roster();
    assert_eq!(
        state_of(&roster, worker).state,
        WorkerRosterStateV1::Stranded
    );
}

/// #3090 (#2054): a `PIPELINE HANDOFF` written while the worker's
/// `jobs_terminal` wake is still owed is provisional. The daemon resumes the
/// worker with the result, so the roster reports the positive parked state, and
/// only a handoff written after the predicate settled (or with no wake owed) is
/// `final_handoff`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_provisional_handoff_beside_an_owed_job_result_wake_stays_parked_3090() {
    const PAST: &str = "2001-01-01T00:00:00.000000000Z";
    const FUTURE: &str = "2999-01-01T00:00:00.000000000Z";
    const HANDOFF: &str = "PIPELINE HANDOFF — IMPLEMENTATION:\nFriction: none";
    let world = World::new();
    let covered = |worker: Uuid| {
        world
            .store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                params![worker.to_string(), world.manager.to_string()],
            )
            .unwrap();
    };
    let settle = |job: Uuid, finished_at: &str| {
        world
            .store
            .conn
            .execute(
                "UPDATE agent_jobs SET state='succeeded', exit_code=0, finished_at=?2 WHERE id=?1",
                params![job.to_string(), finished_at],
            )
            .unwrap();
    };

    // The reported case: the job still runs and the wake is armed.
    let running = world.worker(SessionStatus::Completed, None, T0);
    covered(running);
    let job = world.job(running, "test", "running", None, None);
    world.wake(running, WakeMode::Resume, Some(job_wake_predicate(job)));
    world.say(running, Role::Assistant, HANDOFF);

    // The job ended after the marker and the wake has not fired yet.
    let settled_after = world.worker(SessionStatus::Completed, None, T1);
    covered(settled_after);
    let job = world.job(settled_after, "test", "running", None, None);
    world.wake(
        settled_after,
        WakeMode::Resume,
        Some(job_wake_predicate(job)),
    );
    world.say(settled_after, Role::Assistant, HANDOFF);
    settle(job, FUTURE);

    // The job ended before the marker: the wake is superseded, the result final.
    let settled_before = world.worker(SessionStatus::Completed, None, T2);
    covered(settled_before);
    let job = world.job(settled_before, "test", "running", None, None);
    world.wake(
        settled_before,
        WakeMode::Resume,
        Some(job_wake_predicate(job)),
    );
    settle(job, PAST);
    world.say(settled_before, Role::Assistant, HANDOFF);

    // A valid result with no wake owed.
    let done = world.worker(SessionStatus::Completed, None, T3);
    covered(done);
    world.job(done, "test", "succeeded", Some(0), None);
    world.say(done, Role::Assistant, HANDOFF);

    let roster = world.manager_roster();
    let row = state_of(&roster, running);
    assert_eq!(row.state, WorkerRosterStateV1::ParkedOnJob);
    assert!(!row.final_handoff);
    assert_eq!(row.open_jobs, 1);
    let row = state_of(&roster, settled_after);
    assert_eq!(row.state, WorkerRosterStateV1::ParkedOnWake);
    assert!(!row.final_handoff);
    for worker in [settled_before, done] {
        let row = state_of(&roster, worker);
        assert_eq!(row.state, WorkerRosterStateV1::FinalHandoff, "{worker}");
        assert!(row.final_handoff);
    }
    assert_eq!(
        roster.counts.get(&WorkerRosterStateV1::ParkedOnJob),
        Some(&1)
    );
    assert_eq!(
        roster.counts.get(&WorkerRosterStateV1::ParkedOnWake),
        Some(&1)
    );
    assert_eq!(
        roster.counts.get(&WorkerRosterStateV1::FinalHandoff),
        Some(&2)
    );
    // The daemon's own parked predicate tells the same story.
    for (worker, parked) in [
        (running, true),
        (settled_after, true),
        (settled_before, false),
        (done, false),
    ] {
        assert_eq!(
            world
                .store
                .parked_session_pending(worker)
                .unwrap()
                .is_some(),
            parked,
            "{worker}"
        );
    }
}

/// `Failed` and `Interrupted` are `failed`; the newest job shows its refusal;
/// archived workers are not listed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn failed_and_interrupted_workers_are_failed_and_archived_ones_are_not_listed() {
    let world = World::new();
    let failed = world.worker(SessionStatus::Failed, None, T0);
    world.job(failed, "test", "succeeded", Some(0), None);
    let newest = world.job(
        failed,
        "test",
        "failed",
        Some(1),
        Some(serde_json::json!({"exit_code": 1, "refusal": "job_timed_out"})),
    );
    let interrupted = world.worker(SessionStatus::Interrupted, None, T1);
    let archived = world.worker(SessionStatus::Archived, None, T2);
    let deleted = world.worker(SessionStatus::Deleted, None, T3);

    let roster = world.manager_roster();
    assert_eq!(roster.count, 2);
    for worker in [failed, interrupted] {
        assert_eq!(state_of(&roster, worker).state, WorkerRosterStateV1::Failed);
    }
    let latest = state_of(&roster, failed).latest_job.as_ref().unwrap();
    assert_eq!(latest.job_id, newest);
    assert_eq!(latest.state, JobState::Failed);
    assert_eq!(latest.exit_code, Some(1));
    assert_eq!(latest.refusal.as_deref(), Some("job_timed_out"));
    for gone in [archived, deleted] {
        assert!(roster.workers.iter().all(|w| w.session_id != gone));
    }
}

/// `states` filters, `limit` pages newest-first and says when it truncated.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn states_filter_and_limit_page_the_roster() {
    let world = World::new();
    let running = world.worker(SessionStatus::Running, None, T3);
    let stranded = world.worker(SessionStatus::Completed, None, T2);
    world.say(stranded, Role::Assistant, "idle");
    let failed = world.worker(SessionStatus::Failed, None, T1);

    let only = |states: Vec<WorkerRosterStateV1>, limit: Option<u32>| {
        world
            .roster(
                world.manager,
                AgentWorkerRosterRequestV1 {
                    states,
                    limit,
                    ..AgentWorkerRosterRequestV1::default()
                },
            )
            .unwrap()
    };
    let stranded_only = only(vec![WorkerRosterStateV1::Stranded], None);
    assert_eq!(
        stranded_only
            .workers
            .iter()
            .map(|w| w.session_id)
            .collect::<Vec<_>>(),
        [stranded]
    );
    assert!(!stranded_only.truncated);

    let two = only(
        vec![WorkerRosterStateV1::Stranded, WorkerRosterStateV1::Failed],
        None,
    );
    assert_eq!(
        two.workers.iter().map(|w| w.session_id).collect::<Vec<_>>(),
        [stranded, failed]
    );

    let page = only(vec![], Some(2));
    assert_eq!(
        page.workers
            .iter()
            .map(|w| w.session_id)
            .collect::<Vec<_>>(),
        [running, stranded]
    );
    assert!(page.truncated);
    assert_eq!(page.count, 2);

    let all = only(vec![], Some(3));
    assert_eq!(all.count, 3);
    assert!(!all.truncated);

    let error = world
        .roster(
            world.manager,
            AgentWorkerRosterRequestV1 {
                limit: Some(0),
                ..AgentWorkerRosterRequestV1::default()
            },
        )
        .unwrap_err();
    assert_eq!(code(error), WORKER_ROSTER_INVALID_REQUEST);
}

/// A caller that is not a project's manager lists the workers it launched
/// itself, and naming a project it does not manage is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_non_manager_lists_only_the_workers_it_launched_itself() {
    let world = World::new();
    let manager_worker = world.worker(SessionStatus::Running, None, T1);
    let lead = World::seat(&world.store, Some(world.project), SessionStatus::Running);
    let epic = Uuid::new_v4();
    let mut epic_row = test_session(epic, PathBuf::from("/tmp/roster-epic"));
    epic_row.project_id = Some(world.project);
    epic_row.session_kind = rsi_common::types::SessionKind::Epic;
    epic_row.status = SessionStatus::Running;
    world.store.insert_session(&epic_row).unwrap();
    let spawned = Uuid::new_v4();
    let mut child = test_session(spawned, PathBuf::from("/tmp/roster-child"));
    child.project_id = Some(world.project);
    child.status = SessionStatus::Running;
    world.store.insert_session(&child).unwrap();
    world
        .store
        .conn
        .execute(
            "INSERT INTO agent_spawn_requests(spawn_request_id,owner_session_id,idempotency_digest,
                request_fingerprint,request_json,child_session_id,epic_id,kind,state,
                reserved_at,updated_at,launched_at,epic_spawn_ordinal)
             VALUES(?1,?2,?3,?3,'{}',?4,?5,'Task','launched',?6,?6,?6,1)",
            params![
                Uuid::new_v4().to_string(),
                lead.to_string(),
                format!("sha256:{}", "c".repeat(64)),
                spawned.to_string(),
                epic.to_string(),
                T0
            ],
        )
        .unwrap();

    let own = world
        .roster(lead, AgentWorkerRosterRequestV1::default())
        .unwrap();
    assert_eq!(own.scope.kind, WorkerRosterScopeKindV1::OwnLaunched);
    assert_eq!(own.scope.project_id, None);
    assert_eq!(
        own.workers.iter().map(|w| w.session_id).collect::<Vec<_>>(),
        [spawned]
    );

    // The manager's project view holds both the manager-launched worker and
    // the lead's child, and never the manager itself or the lead.
    let project_view = world.manager_roster();
    let ids: Vec<Uuid> = project_view.workers.iter().map(|w| w.session_id).collect();
    assert!(
        ids.contains(&manager_worker) && ids.contains(&spawned),
        "{ids:?}"
    );
    assert!(!ids.contains(&world.manager) && !ids.contains(&lead) && !ids.contains(&epic));

    // Naming a project the caller does not manage is refused, even its own.
    let error = world
        .roster(
            lead,
            AgentWorkerRosterRequestV1 {
                project_id: Some(world.project),
                ..AgentWorkerRosterRequestV1::default()
            },
        )
        .unwrap_err();
    assert_eq!(code(error), WORKER_ROSTER_NOT_AUTHORIZED);
    // A project's manager may not read another project's workers.
    let other = project(&world.store, "other");
    let error = world
        .roster(
            world.manager,
            AgentWorkerRosterRequestV1 {
                project_id: Some(other),
                ..AgentWorkerRosterRequestV1::default()
            },
        )
        .unwrap_err();
    assert_eq!(code(error), WORKER_ROSTER_NOT_AUTHORIZED);
    // An unknown caller is refused.
    let error = world
        .roster(Uuid::new_v4(), AgentWorkerRosterRequestV1::default())
        .unwrap_err();
    assert_eq!(code(error), WORKER_ROSTER_CALLER_UNAVAILABLE);
}

/// A portfolio seat lists a covered project's workers by naming it, and is
/// refused outside its grant.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_portfolio_seat_lists_a_covered_projects_workers_and_nothing_outside_its_grant() {
    let world = World::new();
    let stranded = world.worker(SessionStatus::Completed, None, T0);
    world.say(stranded, Role::Assistant, "idle");
    let outside = project(&world.store, "outside");
    let seat = World::seat(&world.store, None, SessionStatus::Running);
    world
        .store
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![world.project],
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: Some("high".into()),
                }],
                project_policy: ManagerPolicyV2::default(),
                expected_grant_version: 0,
                idempotency_key: "roster-grant".into(),
            },
            "operator:test",
        )
        .unwrap();

    let covered = world
        .roster(
            seat,
            AgentWorkerRosterRequestV1 {
                project_id: Some(world.project),
                states: vec![WorkerRosterStateV1::Stranded],
                ..AgentWorkerRosterRequestV1::default()
            },
        )
        .unwrap();
    assert_eq!(covered.scope.kind, WorkerRosterScopeKindV1::Project);
    assert_eq!(covered.scope.project_id, Some(world.project));
    assert_eq!(
        covered
            .workers
            .iter()
            .map(|w| w.session_id)
            .collect::<Vec<_>>(),
        [stranded]
    );
    // Without a project the seat lists what it launched itself: nothing.
    let own = world
        .roster(seat, AgentWorkerRosterRequestV1::default())
        .unwrap();
    assert_eq!(own.scope.kind, WorkerRosterScopeKindV1::OwnLaunched);
    assert!(own.workers.is_empty());

    let error = world
        .roster(
            seat,
            AgentWorkerRosterRequestV1 {
                project_id: Some(outside),
                ..AgentWorkerRosterRequestV1::default()
            },
        )
        .unwrap_err();
    assert_eq!(code(error), MANAGER_PROJECT_NOT_IN_SCOPE);
}

/// The roster agrees with the daemon's own liveness predicate for a parked
/// worker, so the two can never tell a manager different things.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn parked_states_agree_with_the_daemons_parked_session_predicate() {
    let world = World::new();
    let workers = [
        world.worker(SessionStatus::Completed, None, T0),
        world.worker(SessionStatus::Completed, None, T1),
        world.worker(SessionStatus::Completed, None, T2),
        world.worker(SessionStatus::Completed, None, T3),
    ];
    for worker in workers {
        world.say(worker, Role::Assistant, "interim");
    }
    let job = world.job(workers[1], "test", "running", None, None);
    world.wake(workers[1], WakeMode::Resume, Some(job_wake_predicate(job)));
    world.wake(workers[2], WakeMode::Resume, None);
    world.say(workers[3], Role::Assistant, "RESULT: done");

    let roster = world.manager_roster();
    for worker in workers {
        let parked = world
            .store
            .parked_session_pending(worker)
            .unwrap()
            .is_some();
        let row = state_of(&roster, worker);
        assert_eq!(
            parked,
            matches!(
                row.state,
                WorkerRosterStateV1::ParkedOnJob | WorkerRosterStateV1::ParkedOnWake
            ),
            "{worker}: {:?}",
            row.state
        );
    }
}

/// #2362: a seat that is not a project manager (the global manager) lists the
/// sessions its own `AgentManagerLaunchIssueWorker` actions created, whose
/// parent is the launch action, and not another launcher's.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn own_launched_scope_includes_workers_the_callers_launch_actions_created() {
    let world = World::new();
    let seat = World::seat(&world.store, None, SessionStatus::Running);
    let other_seat = World::seat(&world.store, None, SessionStatus::Running);
    let launched = world.worker(SessionStatus::Running, None, T1);
    let also_launched = world.worker(SessionStatus::Completed, None, T2);
    world.say(also_launched, Role::Assistant, "idle");
    let someone_elses = world.worker(SessionStatus::Running, None, T3);
    let unlaunched = world.worker(SessionStatus::Running, None, T0);
    for (worker, actor) in [
        (launched, seat),
        (also_launched, seat),
        (someone_elses, other_seat),
    ] {
        world
            .store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET actor_session_id=?2
                 WHERE target_session_id=?1",
                params![worker.to_string(), actor.to_string()],
            )
            .unwrap();
    }

    let own = world
        .roster(seat, AgentWorkerRosterRequestV1::default())
        .unwrap();
    assert_eq!(own.scope.kind, WorkerRosterScopeKindV1::OwnLaunched);
    let ids: Vec<Uuid> = own.workers.iter().map(|w| w.session_id).collect();
    assert_eq!(ids, [also_launched, launched]);
    assert!(!ids.contains(&someone_elses) && !ids.contains(&unlaunched));
    assert_eq!(own.workers[0].state, WorkerRosterStateV1::Stranded);
    assert_eq!(own.workers[1].state, WorkerRosterStateV1::Running);

    // A launch the daemon never ran (failed) does not make a worker the seat's.
    world
        .store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations SET state='failed' WHERE target_session_id=?1",
            [launched.to_string()],
        )
        .unwrap();
    let own = world
        .roster(seat, AgentWorkerRosterRequestV1::default())
        .unwrap();
    assert_eq!(
        own.workers.iter().map(|w| w.session_id).collect::<Vec<_>>(),
        [also_launched]
    );
}

/// #2362: a completed worker that published an Issue handoff but omitted the
/// `PIPELINE HANDOFF` marker is `final_handoff`, not `stranded`; a worker that
/// published nothing, or one that is still parked, is unchanged.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_worker_that_published_an_issue_handoff_without_the_marker_is_final_not_stranded() {
    let world = World::new();
    let issue = world.issue();
    let reviewer = world.worker(SessionStatus::Completed, Some(issue), T1);
    world.say(reviewer, Role::Assistant, "Review complete: approve.");
    let silent = world.worker(SessionStatus::Completed, Some(issue), T2);
    world.say(silent, Role::Assistant, "Review complete: approve.");
    let parked = world.worker(SessionStatus::Completed, Some(issue), T3);
    world.say(parked, Role::Assistant, "waiting");
    world.wake(parked, WakeMode::Resume, None);
    for (sequence, author) in [(1, reviewer), (2, parked)] {
        world
            .store
            .conn
            .execute(
                "INSERT INTO issue_handoffs
                     (issue_id, sequence, project_id, author_session_id, idempotency_key,
                      body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'verdict: approve', ?6)",
                params![
                    issue.to_string(),
                    sequence,
                    world.project.to_string(),
                    author.to_string(),
                    format!("handoff-{sequence}"),
                    T3
                ],
            )
            .unwrap();
    }

    let roster = world.manager_roster();
    let row = state_of(&roster, reviewer);
    assert_eq!(row.state, WorkerRosterStateV1::FinalHandoff);
    assert!(row.final_handoff);
    assert_eq!(
        state_of(&roster, silent).state,
        WorkerRosterStateV1::Stranded
    );
    assert_eq!(
        state_of(&roster, parked).state,
        WorkerRosterStateV1::ParkedOnWake
    );
}

/// #2362: a `stranded` worker idle for over a week drops out of the default
/// view and is counted; naming `stranded` lists it, and a fresh strand stays.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn stale_strands_leave_the_default_view_but_not_an_explicit_stranded_filter() {
    let world = World::new();
    let old = world.worker(
        SessionStatus::Completed,
        None,
        "2026-10-01T00:00:00.000000000Z",
    );
    world.say(old, Role::Assistant, "idle");
    let fresh = world.worker(
        SessionStatus::Completed,
        None,
        "2026-10-10T00:00:00.000000000Z",
    );
    world.say(fresh, Role::Assistant, "idle");
    for (worker, at) in [
        (old, "2026-10-01T00:00:00.000000000Z"),
        (fresh, "2026-10-10T00:00:00.000000000Z"),
    ] {
        world
            .store
            .conn
            .execute(
                "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                params![worker.to_string(), at],
            )
            .unwrap();
    }

    let default = world.manager_roster();
    assert_eq!(
        default
            .workers
            .iter()
            .map(|w| w.session_id)
            .collect::<Vec<_>>(),
        [fresh]
    );
    assert_eq!(default.stale_stranded_hidden, 1);
    assert_eq!(default.counts.get(&WorkerRosterStateV1::Stranded), Some(&1));

    let explicit = world
        .roster(
            world.manager,
            AgentWorkerRosterRequestV1 {
                states: vec![WorkerRosterStateV1::Stranded],
                ..AgentWorkerRosterRequestV1::default()
            },
        )
        .unwrap();
    assert_eq!(
        explicit
            .workers
            .iter()
            .map(|w| w.session_id)
            .collect::<Vec<_>>(),
        [fresh, old]
    );
    assert_eq!(explicit.stale_stranded_hidden, 0);
}

fn wake_ids(views: &[rsi_common::agent_wake_list::AgentWakeV1]) -> Vec<Uuid> {
    views.iter().map(|view| view.job_id).collect()
}

fn read_wakes(store: &Store, caller: Uuid, target: Option<Uuid>) -> Result<Vec<Uuid>> {
    store
        .list_wake_views_for_reader(caller, target, false, 64)
        .map(|views| wake_ids(&views))
}

const LIST_WAKES_REFUSED: &str = rsi_common::agent_wake_list::LIST_WAKES_TARGET_NOT_AUTHORIZED;

/// #2418: the default read stays caller-bound for every caller, a worker
/// naming itself is the same read, and the manager reads the live worker's
/// armed wake after the worker handed off, by the roster's own rule.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn list_wakes_serves_own_wakes_and_a_managers_live_worker_wakes_after_handoff() {
    let world = World::new();
    let worker = world.worker(SessionStatus::Completed, None, T1);
    world.say(worker, Role::Assistant, "RESULT: done");
    let worker_wake = world.wake(worker, WakeMode::Resume, None);
    let manager_wake = world.wake(world.manager, WakeMode::Resume, None);

    // The roster lists the worker; the wake read agrees.
    assert_eq!(state_of(&world.manager_roster(), worker).session_id, worker);
    // Default and explicit-self reads are caller-bound and unchanged.
    assert_eq!(
        read_wakes(&world.store, world.manager, None).unwrap(),
        [manager_wake]
    );
    assert_eq!(
        read_wakes(&world.store, world.manager, Some(world.manager)).unwrap(),
        [manager_wake]
    );
    assert_eq!(
        read_wakes(&world.store, worker, None).unwrap(),
        [worker_wake]
    );
    assert_eq!(
        read_wakes(&world.store, worker, Some(worker)).unwrap(),
        [worker_wake]
    );
    // The manager reads the worker's wake, and only that worker's.
    assert_eq!(
        read_wakes(&world.store, world.manager, Some(worker)).unwrap(),
        [worker_wake]
    );
    // The worker cannot read its manager's wakes (no upward authority).
    assert_eq!(
        code(read_wakes(&world.store, worker, Some(world.manager)).unwrap_err()),
        LIST_WAKES_REFUSED
    );
}

/// #2418: foreign, unknown, archived and out-of-project sessions are refused,
/// and a launcher reads only the workers it launched.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn list_wakes_refuses_foreign_unknown_and_archived_targets() {
    let world = World::new();
    let mine = world.worker(SessionStatus::Running, None, T1);
    world.wake(mine, WakeMode::Resume, None);
    // A worker in another project, with its own manager.
    let other = project(&world.store, "other");
    let foreign = Uuid::new_v4();
    let mut row = test_session(foreign, PathBuf::from("/tmp/roster-foreign"));
    row.project_id = Some(other);
    row.status = SessionStatus::Running;
    world.store.insert_session(&row).unwrap();
    world.wake(foreign, WakeMode::Resume, None);
    // A plain session of the manager's project that no launch created.
    let unlaunched = World::seat(&world.store, Some(world.project), SessionStatus::Running);
    world.wake(unlaunched, WakeMode::Resume, None);

    for target in [foreign, unlaunched, Uuid::new_v4()] {
        assert_eq!(
            code(read_wakes(&world.store, world.manager, Some(target)).unwrap_err()),
            LIST_WAKES_REFUSED,
            "{target}"
        );
    }
    // A non-manager session of the same project reads neither the manager's
    // worker nor the manager.
    let peer = World::seat(&world.store, Some(world.project), SessionStatus::Running);
    for target in [mine, world.manager] {
        assert_eq!(
            code(read_wakes(&world.store, peer, Some(target)).unwrap_err()),
            LIST_WAKES_REFUSED
        );
    }
    // An unknown caller is refused for another session's wakes.
    assert_eq!(
        code(read_wakes(&world.store, Uuid::new_v4(), Some(mine)).unwrap_err()),
        WORKER_ROSTER_CALLER_UNAVAILABLE
    );
    // An archived worker is not live scope, wakes or not.
    world
        .store
        .conn
        .execute(
            "UPDATE sessions SET status='Archived' WHERE id=?1",
            [mine.to_string()],
        )
        .unwrap();
    assert_eq!(
        code(read_wakes(&world.store, world.manager, Some(mine)).unwrap_err()),
        LIST_WAKES_REFUSED
    );
}

/// #2418: a launcher that is not a project manager reads the wakes of the
/// workers it launched itself and of no other session.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn list_wakes_serves_a_non_managers_own_launched_workers_only() {
    let world = World::new();
    let seat = World::seat(&world.store, None, SessionStatus::Running);
    let launched = world.worker(SessionStatus::Running, None, T1);
    let someone_elses = world.worker(SessionStatus::Running, None, T2);
    world
        .store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations SET actor_session_id=?2
             WHERE target_session_id=?1",
            params![launched.to_string(), seat.to_string()],
        )
        .unwrap();
    let wake = world.wake(launched, WakeMode::Resume, None);
    world.wake(someone_elses, WakeMode::Resume, None);

    assert_eq!(
        read_wakes(&world.store, seat, Some(launched)).unwrap(),
        [wake]
    );
    assert_eq!(
        code(read_wakes(&world.store, seat, Some(someone_elses)).unwrap_err()),
        LIST_WAKES_REFUSED
    );
}

/// #2418: authority is live. After the manager seat is replaced the old seat is
/// refused and the successor is served; a launch that failed no longer makes a
/// worker the caller's.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn list_wakes_authority_follows_the_live_manager_seat() {
    let world = World::new();
    let worker = world.worker(SessionStatus::Running, None, T1);
    let wake = world.wake(worker, WakeMode::Resume, None);
    assert_eq!(
        read_wakes(&world.store, world.manager, Some(worker)).unwrap(),
        [wake]
    );

    let successor = Uuid::new_v4();
    let mut row = test_session(successor, PathBuf::from("/tmp/roster-successor"));
    row.project_id = Some(world.project);
    row.status = SessionStatus::Running;
    row.continued_from = Some(world.manager);
    world.store.insert_session(&row).unwrap();
    // The committed rotation edge makes the successor the lineage tip once the
    // predecessor is archived.
    world
        .store
        .conn
        .execute(
            "INSERT INTO harness_manager_rotation_edges
                (predecessor_session_id,successor_session_id,committed_at)
             VALUES(?1,?2,?3)",
            params![world.manager.to_string(), successor.to_string(), T2],
        )
        .unwrap();
    world
        .store
        .conn
        .execute(
            "UPDATE sessions SET status='Archived' WHERE id=?1",
            [world.manager.to_string()],
        )
        .unwrap();

    // The displaced seat holds no live seat any more: refused, whatever the code.
    assert!(read_wakes(&world.store, world.manager, Some(worker)).is_err());
    assert_eq!(
        read_wakes(&world.store, successor, Some(worker)).unwrap(),
        [wake]
    );
}

/// #2418: a portfolio seat reads a covered project's worker wakes, never
/// outside its grant, and a narrowed grant stops the read.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn list_wakes_follows_a_portfolio_seats_grant_and_its_narrowing() {
    let world = World::new();
    let worker = world.worker(SessionStatus::Running, None, T1);
    let wake = world.wake(worker, WakeMode::Resume, None);
    let outside = project(&world.store, "outside");
    let seat = World::seat(&world.store, None, SessionStatus::Running);
    let grant = |projects: Vec<Uuid>, expected: i64, key: &str| ConfigureGlobalManagerRequestV1 {
        session_id: seat,
        project_ids: projects,
        allowed_launches: vec![ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-opus-5-5".into(),
            effort: Some("high".into()),
        }],
        project_policy: ManagerPolicyV2::default(),
        expected_grant_version: expected,
        idempotency_key: key.into(),
    };
    world
        .store
        .configure_global_manager(
            &grant(vec![world.project], 0, "wakes-grant-1"),
            "operator:test",
        )
        .unwrap();
    assert_eq!(
        read_wakes(&world.store, seat, Some(worker)).unwrap(),
        [wake]
    );

    // Narrowed to a different project: the same worker is out of scope.
    world
        .store
        .configure_global_manager(&grant(vec![outside], 1, "wakes-grant-2"), "operator:test")
        .unwrap();
    assert_eq!(
        code(read_wakes(&world.store, seat, Some(worker)).unwrap_err()),
        LIST_WAKES_REFUSED
    );
}

impl World {
    /// An Issue the manager has started: `InProgress`, as a launch leaves it.
    fn in_progress_issue(&self) -> Uuid {
        let issue = self.issue();
        self.set_issue_status(issue, "InProgress");
        issue
    }

    fn set_issue_status(&self, issue: Uuid, status: &str) {
        self.store
            .conn
            .execute(
                "UPDATE issues SET status=?2 WHERE id=?1",
                params![issue.to_string(), status],
            )
            .unwrap();
    }

    /// An Issue handoff of `author`, stamped `created_at`.
    fn publish_handoff(&self, issue: Uuid, author: Uuid, sequence: i64, created_at: &str) {
        self.store
            .conn
            .execute(
                "INSERT INTO issue_handoffs
                     (issue_id, sequence, project_id, author_session_id, idempotency_key,
                      body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'finished', ?6)",
                params![
                    issue.to_string(),
                    sequence,
                    self.project.to_string(),
                    author.to_string(),
                    format!("handoff-247-{sequence}"),
                    created_at
                ],
            )
            .unwrap();
    }
}

/// #247: a worker bound to an `InProgress` Issue with no handoff and no final
/// report holds its sandbox, whatever its terminal-looking status. Every end
/// of the work releases it: a final report, a published handoff, a closed
/// Issue, a successor launch. An unbound session never holds.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn an_unfinished_issue_worker_holds_until_it_reports_or_its_issue_ends() {
    let world = World::new();

    let issue = world.in_progress_issue();
    let halted = world.worker(SessionStatus::Interrupted, Some(issue), T1);
    world.say(halted, Role::Assistant, "partway through the change");
    assert!(world.store.unfinished_issue_worker_hold(halted).unwrap());
    assert_eq!(
        world.store.automatic_reclaim_resume_hold(halted).unwrap(),
        Some("unfinished_issue_worker")
    );

    let issue = world.in_progress_issue();
    let reported = world.worker(SessionStatus::Interrupted, Some(issue), T1);
    world.say(
        reported,
        Role::Assistant,
        "PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT: done",
    );
    assert!(!world.store.unfinished_issue_worker_hold(reported).unwrap());
    assert_eq!(
        world.store.automatic_reclaim_resume_hold(reported).unwrap(),
        None
    );

    let issue = world.in_progress_issue();
    let published = world.worker(SessionStatus::Interrupted, Some(issue), T1);
    assert!(world.store.unfinished_issue_worker_hold(published).unwrap());
    // An Issue handoff alone is progress, not completion: only the worker's own
    // final report in its latest turn finishes it.
    world.publish_handoff(issue, published, 1, T3);
    assert!(world.store.unfinished_issue_worker_hold(published).unwrap());
    world.say(
        published,
        Role::Assistant,
        "PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT: done",
    );
    assert!(!world.store.unfinished_issue_worker_hold(published).unwrap());

    let issue = world.in_progress_issue();
    let closed = world.worker(SessionStatus::Interrupted, Some(issue), T1);
    assert!(world.store.unfinished_issue_worker_hold(closed).unwrap());
    world.set_issue_status(issue, "Closed");
    assert!(!world.store.unfinished_issue_worker_hold(closed).unwrap());

    let issue = world.in_progress_issue();
    let predecessor = world.worker(SessionStatus::Interrupted, Some(issue), T1);
    assert!(
        world
            .store
            .unfinished_issue_worker_hold(predecessor)
            .unwrap()
    );
    let successor = world.worker(SessionStatus::Running, Some(issue), T2);
    assert!(
        !world
            .store
            .unfinished_issue_worker_hold(predecessor)
            .unwrap()
    );
    assert!(world.store.unfinished_issue_worker_hold(successor).unwrap());

    let unbound = world.worker(SessionStatus::Interrupted, None, T1);
    assert!(!world.store.unfinished_issue_worker_hold(unbound).unwrap());
}

/// #247: an Issue handoff (pre-verification, WIP or baton) is progress, never
/// completion. Whether it is old (before the manager's Continue) or written in
/// the current turn, the worker stays unfinished until its latest turn carries
/// a final report (the existing report contract), its Issue ends or a successor
/// takes over.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn an_issue_handoff_alone_never_releases_a_worker_without_a_current_final_report() {
    let world = World::new();

    // Pre-verification handoff and report, then a manager Continue, then a
    // halt for higher-priority work: nothing in the current turn is final.
    let issue = world.in_progress_issue();
    let worker = world.worker(SessionStatus::Interrupted, Some(issue), T1);
    world.say(worker, Role::User, "launch brief");
    world.say(
        worker,
        Role::Assistant,
        "PIPELINE HANDOFF — IMPLEMENTATION:\nPREVERIFICATION: tests not yet run",
    );
    world.publish_handoff(issue, worker, 1, T3);
    assert!(
        !world.store.unfinished_issue_worker_hold(worker).unwrap(),
        "the latest turn still carries the report before any Continue"
    );
    world.say(worker, Role::User, "Continue: correct the review finding");
    assert!(world.store.unfinished_issue_worker_hold(worker).unwrap());
    assert_eq!(
        world.store.automatic_reclaim_resume_hold(worker).unwrap(),
        Some("unfinished_issue_worker")
    );

    // A mid-turn assistant message that is not a report changes nothing, and a
    // handoff published in THIS turn without a final report still holds.
    world.say(worker, Role::Assistant, "applying the correction");
    assert!(world.store.unfinished_issue_worker_hold(worker).unwrap());
    world.publish_handoff(issue, worker, 2, "2099-01-01T00:00:00.000000000Z");
    assert!(world.store.unfinished_issue_worker_hold(worker).unwrap());

    // The current turn's final report releases it.
    world.say(
        worker,
        Role::Assistant,
        "PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT: corrected",
    );
    assert!(!world.store.unfinished_issue_worker_hold(worker).unwrap());

    // A same-turn WIP handoff with no User message and no report holds too.
    let issue = world.in_progress_issue();
    let wip = world.worker(SessionStatus::Interrupted, Some(issue), T1);
    world.say(
        wip,
        Role::Assistant,
        "baton written, halted before finishing",
    );
    world.publish_handoff(issue, wip, 1, "2099-01-01T00:00:00.000000000Z");
    assert!(world.store.unfinished_issue_worker_hold(wip).unwrap());
}

/// An Epic under the world's project with no lead.
fn leadless_epic(world: &World) -> Uuid {
    let id = Uuid::new_v4();
    let mut epic = test_session(id, PathBuf::from("/tmp/roster-epic"));
    epic.session_kind = rsi_common::types::SessionKind::Epic;
    epic.project_id = Some(world.project);
    world.store.insert_session(&epic).unwrap();
    id
}

/// A rotation child of `predecessor` (`continued_from`).
fn rotation_child(world: &World, predecessor: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let mut child = test_session(id, PathBuf::from("/tmp/roster-child"));
    child.project_id = Some(world.project);
    child.continued_from = Some(predecessor);
    world.store.insert_session(&child).unwrap();
    id
}

/// #2804: an Issue-bound worker is never an Epic's lead, by auto-promotion or
/// by an explicit assignment, while a plain child is still auto-promoted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn an_issue_bound_worker_is_never_an_epic_lead_but_a_plain_child_is() {
    let world = World::new();
    let epic = leadless_epic(&world);
    let issue = world.issue();
    let bound = world.worker(SessionStatus::Running, Some(issue), T1);
    let plain = world.worker(SessionStatus::Running, None, T1);

    assert!(world.store.is_non_lead_worker_session(bound).unwrap());
    assert!(!world.store.is_non_lead_worker_session(plain).unwrap());
    assert!(!world.store.try_promote_lead_if_unset(epic, bound).unwrap());
    assert!(world.store.set_lead_session(epic, Some(bound)).is_err());
    assert_eq!(
        world
            .store
            .get_session(epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        None
    );

    assert!(world.store.try_promote_lead_if_unset(epic, plain).unwrap());
    assert_eq!(
        world
            .store
            .get_session(epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(plain)
    );
}

/// #2805: a rotation child of an Issue-bound or topology worker is a worker
/// too, at any depth, and a lineage cycle ends the walk.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_rotation_child_of_a_bound_or_topology_worker_is_never_an_epic_lead() {
    let world = World::new();
    let epic = leadless_epic(&world);
    let issue = world.issue();
    let bound = world.worker(SessionStatus::Completed, Some(issue), T0);
    let child = rotation_child(&world, bound);
    let grandchild = rotation_child(&world, child);
    for id in [child, grandchild] {
        assert!(world.store.is_non_lead_worker_session(id).unwrap());
        assert!(!world.store.try_promote_lead_if_unset(epic, id).unwrap());
        assert!(world.store.set_lead_session(epic, Some(id)).is_err());
    }

    let node = world.worker(SessionStatus::Completed, None, T0);
    let execution = Uuid::new_v4();
    let now = "2026-10-02T12:00:00.000000000Z";
    world
        .store
        .conn
        .execute(
            "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id,definition_json,definition_digest,requested_by_kind,repo_root,base_commit,custody_plan_json,status,created_at,updated_at) \
             VALUES (?1,'pipeline','wf','{}','d','manager','/tmp/repo','0000000000000000000000000000000000000000','{}','running',?2,?2)",
            params![execution.to_string(), now],
        )
        .unwrap();
    world
        .store
        .conn
        .execute(
            "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,session_id,input_json,created_at,updated_at) \
             VALUES (?1,?2,'implement',0,1,'session','running',?3,?4,'{}',?5,?5)",
            params![
                Uuid::new_v4().to_string(),
                execution.to_string(),
                format!("topology.node:{execution}:implement:0:1"),
                node.to_string(),
                now
            ],
        )
        .unwrap();
    let topology_child = rotation_child(&world, node);
    assert!(
        world
            .store
            .is_non_lead_worker_session(topology_child)
            .unwrap()
    );
    assert!(
        !world
            .store
            .try_promote_lead_if_unset(epic, topology_child)
            .unwrap()
    );

    // A lineage cycle neither hangs nor makes a plain session a worker.
    let a = world.worker(SessionStatus::Completed, None, T0);
    let b = rotation_child(&world, a);
    world
        .store
        .conn
        .execute(
            "UPDATE sessions SET continued_from=?1 WHERE id=?2",
            params![b.to_string(), a.to_string()],
        )
        .unwrap();
    assert!(!world.store.is_non_lead_worker_session(b).unwrap());
}

impl World {
    /// A topology execution in `project` requested by `requester`, with one
    /// session node whose attempt ran `node`. The node session is not one the
    /// manager's `create_session` or spawn paths created (#2914).
    fn topology_node_session(
        &self,
        project: Option<Uuid>,
        requester: Option<Uuid>,
        node_project: Option<Uuid>,
    ) -> Uuid {
        let node = Uuid::new_v4();
        let mut row = test_session(node, PathBuf::from("/tmp/roster-topology-node"));
        row.project_id = node_project;
        row.status = SessionStatus::Running;
        self.store.insert_session(&row).unwrap();
        let execution = Uuid::new_v4();
        let now = "2026-10-02T12:00:00.000000000Z";
        self.store
            .conn
            .execute(
                "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id,definition_json,definition_digest,project_id,requested_by_kind,requested_by_session_id,repo_root,base_commit,custody_plan_json,status,created_at,updated_at) \
                 VALUES (?1,'pipeline','wf','{}','d',?2,'manager',?3,'/tmp/repo','0000000000000000000000000000000000000000','{}','running',?4,?4)",
                params![
                    execution.to_string(),
                    project.map(|id| id.to_string()),
                    requester.map(|id| id.to_string()),
                    now
                ],
            )
            .unwrap();
        self.store
            .conn
            .execute(
                "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,session_id,input_json,created_at,updated_at) \
                 VALUES (?1,?2,'implement',0,1,'session','running',?3,?4,'{}',?5,?5)",
                params![
                    Uuid::new_v4().to_string(),
                    execution.to_string(),
                    format!("topology.node:{execution}:implement:0:1"),
                    node.to_string(),
                    now
                ],
            )
            .unwrap();
        node
    }
}

/// #2914: a project manager reads the wakes (and roster rows) of the node
/// sessions of its project's topology executions, and of no other execution's.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn list_wakes_serves_topology_node_sessions_of_the_managers_project_only() {
    let world = World::new();
    // A node whose session carries no project at all: scope comes from the
    // execution.
    let node = world.topology_node_session(Some(world.project), Some(world.manager), None);
    let wake = world.wake(node, WakeMode::Resume, None);
    assert_eq!(
        read_wakes(&world.store, world.manager, Some(node)).unwrap(),
        [wake]
    );
    assert_eq!(state_of(&world.manager_roster(), node).session_id, node);

    // An execution of another project is out of scope, even when it ran a node.
    let other = project(&world.store, "other-topology");
    let foreign = world.topology_node_session(Some(other), None, Some(other));
    world.wake(foreign, WakeMode::Resume, None);
    assert_eq!(
        code(read_wakes(&world.store, world.manager, Some(foreign)).unwrap_err()),
        LIST_WAKES_REFUSED
    );
    // A peer session of the project holds no seat: refused.
    let peer = World::seat(&world.store, Some(world.project), SessionStatus::Running);
    assert_eq!(
        code(read_wakes(&world.store, peer, Some(node)).unwrap_err()),
        LIST_WAKES_REFUSED
    );
}

/// #2914: a non-manager (an Epic lead) reads the node sessions of the
/// executions it requested, not those another session requested.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn list_wakes_serves_an_epic_leads_own_execution_nodes_only() {
    let world = World::new();
    let lead = World::seat(&world.store, None, SessionStatus::Running);
    let mine = world.topology_node_session(None, Some(lead), None);
    let theirs = world.topology_node_session(None, Some(world.manager), None);
    let wake = world.wake(mine, WakeMode::Resume, None);
    world.wake(theirs, WakeMode::Resume, None);
    assert_eq!(read_wakes(&world.store, lead, Some(mine)).unwrap(), [wake]);
    assert_eq!(
        code(read_wakes(&world.store, lead, Some(theirs)).unwrap_err()),
        LIST_WAKES_REFUSED
    );
}
