//! #2261 acceptance tests: the `topology_execution_settled` wake predicate
//! wakes the owner once when an execution succeeds, fails, is cancelled or
//! blocks. A child module of `agent_tests`, so it shares its `World` (a real
//! manager, Epic leads and a provider-free executor) and drives the real
//! scheduler fast lane.

use super::issue_close_tests::{issue_now, open_issue, status_events, topology_events};
use super::starters_tests::{
    deliver_verified, deliver_verified_at, land_waiting_on, live_grant, pipeline_manager,
    start_pipeline, start_starter, starter_named, waiting_review,
};
use super::*;
use crate::bus::EventBus;
use crate::issue_tracker::poller::SessionLauncher;
use crate::scheduler::wake_when_tests::RecordingLauncher;
use crate::session::harness::tools::schedule_wake::{WakeWhenRequest, build_wake_when_job};
use crate::store::rolling_queue::NewQueueEntry;
use crate::topology::land::LandStatus;
use crate::topology::starters::ISSUE_PIPELINE_REVIEWED;
use rsi_common::rolling_queue::{RollingQueueBinding, RollingQueueEntryState, RollingQueueOutcome};
use rsi_common::types::IssueStatus;
use rsi_common::types::ScheduledJob;
use rsi_common::wake_predicate::WakePredicate;
use std::sync::Arc;

fn settled(execution_id: Uuid) -> WakePredicate {
    WakePredicate {
        topology_execution_settled: Some(execution_id),
        ..WakePredicate::default()
    }
}

/// Arm the wake exactly as `AgentScheduleWake` does, for `owner`.
async fn arm(world: &World, owner: Uuid, execution_id: Uuid) -> Result<ScheduledJob> {
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "execution settled?".into(),
        name: None,
        predicate: settled(execution_id),
        timeout_seconds: None,
        working_dir: world.harness.repo.clone(),
        provider: None,
        model: None,
        project_id: Some(world.project),
        origin_session_id: owner,
    })
    .expect("builds");
    let store = world.harness.executor.store.lock().await;
    crate::scheduler::arm_authorized(&store, &job, &state, false)?;
    Ok(job)
}

struct Lane {
    bus: Arc<EventBus>,
    launcher: Arc<RecordingLauncher>,
}

impl Lane {
    fn new() -> Self {
        Self {
            bus: Arc::new(EventBus::new(64)),
            launcher: Arc::new(RecordingLauncher::default()),
        }
    }

    /// Make the row due (the fast lane's spacing) and run one pass.
    async fn pass(&self, world: &World, wake: Uuid) {
        let store = &world.harness.executor.store;
        store
            .lock()
            .await
            .defer_wake_when(wake, chrono::Utc::now() - chrono::Duration::seconds(1))
            .expect("defer");
        let launcher: Arc<dyn SessionLauncher> = self.launcher.clone();
        crate::scheduler::process_due_wake_when(store, &self.bus, &launcher).await;
    }
}

async fn started(world: &World, name: &str) -> Uuid {
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request(name, definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    world
        .execute(world.lead_a, &flow, world.epic_a, name)
        .await
        .unwrap()
        .execution_id
}

async fn settle(world: &World, execution_id: Uuid, to: ExecutionStatus, error: Option<&str>) {
    let store = world.harness.executor.store.lock().await;
    rows::transition_execution(
        &store,
        execution_id,
        &[ExecutionStatus::Accepted, ExecutionStatus::Running],
        to,
        error.map(str::to_owned),
        None,
        None,
    )
    .unwrap()
    .expect("transitioned");
}

/// `AgentTopologyExecute` as the verb handler runs it: accept, then the
/// requester's default settle wake (hub #2967). Returns the receipt and the
/// armed flag.
async fn execute_with_default_wake(
    world: &World,
    flow: &AgentTopologyUpsertResultV1,
    key: &str,
    wake: Option<rsi_common::agent_jobs::JobWake>,
) -> (
    rsi_common::topology_agent::AgentTopologyExecuteResultV1,
    bool,
) {
    let (request, executed) = accept_only(world, flow, key, wake).await;
    let armed = arm_default(world, &request, &executed).await;
    (executed.result, armed)
}

/// Accept the execution without arming the default wake, so a test can place
/// a manual arm between the two steps.
async fn accept_only(
    world: &World,
    flow: &AgentTopologyUpsertResultV1,
    key: &str,
    wake: Option<rsi_common::agent_jobs::JobWake>,
) -> (AgentTopologyExecuteRequestV1, agent::Executed) {
    let request = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest.clone(),
        epic_id: world.epic_a,
        inputs: serde_json::Value::Null,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: key.into(),
        supersedes_execution: None,
        wake,
    };
    let executed = agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        world.lead_a,
        &request,
    )
    .await
    .expect("accepted");
    (request, executed)
}

async fn arm_default(
    world: &World,
    request: &AgentTopologyExecuteRequestV1,
    executed: &agent::Executed,
) -> bool {
    arm_default_full(world, request, executed).await.armed
}

async fn arm_default_full(
    world: &World,
    request: &AgentTopologyExecuteRequestV1,
    executed: &agent::Executed,
) -> agent::SettleWakeArm {
    let seat = agent::SettleWakeSeat {
        caller: world.lead_a,
        working_dir: world.harness.repo.clone(),
        provider: None,
        model: None,
        project_id: Some(world.project),
    };
    let store = world.harness.executor.store.lock().await;
    agent::arm_settle_wake(&store, seat, request, executed)
}

/// Every settle wake row (enabled or not) of `owner` for the execution.
async fn owned_settle_rows(world: &World, owner: Uuid, execution_id: Uuid) -> Vec<ScheduledJob> {
    let store = world.harness.executor.store.lock().await;
    store
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .filter(|job| job.wake_session_id == Some(owner))
        .filter(|job| {
            store.wake_when_state(job.id).unwrap().is_some_and(|state| {
                state.predicate.topology_execution_settled == Some(execution_id)
            })
        })
        .collect()
}

/// Arm a manual settle wake as `AgentScheduleWake` does (`name` explicit or not),
/// returning the row and the ids it replaced.
async fn arm_manual(
    world: &World,
    owner: Uuid,
    execution_id: Uuid,
    name: Option<&str>,
) -> (ScheduledJob, Vec<Uuid>) {
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "execution settled?".into(),
        name: name.map(str::to_owned),
        predicate: settled(execution_id),
        timeout_seconds: None,
        working_dir: world.harness.repo.clone(),
        provider: None,
        model: None,
        project_id: Some(world.project),
        origin_session_id: owner,
    })
    .expect("builds");
    let store = world.harness.executor.store.lock().await;
    let replaced =
        crate::scheduler::arm_authorized(&store, &job, &state, name.is_some()).expect("armed");
    (job, replaced)
}

/// Settle the execution, run every wake row's pass twice and return the
/// number of resumes the requester got.
async fn resumes_after_settle(world: &World, owner: Uuid, execution_id: Uuid) -> usize {
    let lane = Lane::new();
    settle(world, execution_id, ExecutionStatus::Succeeded, None).await;
    for row in owned_settle_rows(world, owner, execution_id).await {
        lane.pass(world, row.id).await;
        lane.pass(world, row.id).await;
    }
    lane.launcher.deliveries().len()
}

/// The enabled settle wakes armed for `execution_id`.
async fn settle_wakes(world: &World, execution_id: Uuid) -> Vec<ScheduledJob> {
    let name = format!("topology-settled-{execution_id}");
    let store = world.harness.executor.store.lock().await;
    store
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .filter(|job| job.enabled && job.name == name)
        .collect()
}

async fn flow_named(world: &World, name: &str) -> AgentTopologyUpsertResultV1 {
    world
        .upsert(
            world.lead_a,
            &upsert_request(name, definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_execute_without_wake_arms_one_settle_wake_that_resumes_the_requester() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let flow = flow_named(&world, "default-wake").await;
    let (result, armed) = execute_with_default_wake(&world, &flow, "default-wake", None).await;
    assert!(armed, "the default arms the requester's settle wake");
    assert!(!result.deduplicated);
    let wakes = settle_wakes(&world, result.execution_id).await;
    assert_eq!(wakes.len(), 1, "exactly one settle wake");
    assert_eq!(wakes[0].wake_session_id, Some(world.lead_a));

    lane.pass(&world, wakes[0].id).await;
    assert!(lane.launcher.deliveries().is_empty(), "still in flight");
    settle(
        &world,
        result.execution_id,
        ExecutionStatus::Succeeded,
        None,
    )
    .await;
    lane.pass(&world, wakes[0].id).await;
    lane.pass(&world, wakes[0].id).await;
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "the requester resumes exactly once");
    assert_eq!(deliveries[0].0, world.lead_a);
    assert!(
        deliveries[0].1.contains(&format!(
            "execution_id={} status=succeeded",
            result.execution_id
        )),
        "{}",
        deliveries[0].1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_idempotent_execute_replay_arms_no_second_settle_wake() {
    let world = World::new(automation()).await;
    let flow = flow_named(&world, "replay-wake").await;
    let (first, armed) = execute_with_default_wake(&world, &flow, "replay-wake", None).await;
    assert!(armed);
    let (replay, replay_armed) =
        execute_with_default_wake(&world, &flow, "replay-wake", None).await;
    assert!(replay.deduplicated);
    assert_eq!(replay.execution_id, first.execution_id);
    assert!(!replay_armed, "a replay arms nothing");
    assert_eq!(settle_wakes(&world, first.execution_id).await.len(), 1);

    // Even after the first wake was spent on a settled execution.
    let wake = settle_wakes(&world, first.execution_id).await.remove(0);
    settle(
        &world,
        first.execution_id,
        ExecutionStatus::Failed,
        Some("x"),
    )
    .await;
    let lane = Lane::new();
    lane.pass(&world, wake.id).await;
    let (_, late_armed) = execute_with_default_wake(&world, &flow, "replay-wake", None).await;
    assert!(!late_armed);
    assert!(settle_wakes(&world, first.execution_id).await.is_empty());
    assert_eq!(lane.launcher.deliveries().len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_execute_with_wake_none_arms_no_settle_wake() {
    let world = World::new(automation()).await;
    let flow = flow_named(&world, "none-wake").await;
    let (result, armed) = execute_with_default_wake(
        &world,
        &flow,
        "none-wake",
        Some(rsi_common::agent_jobs::JobWake::None),
    )
    .await;
    assert!(!armed);
    assert!(settle_wakes(&world, result.execution_id).await.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_manual_settle_wake_before_the_default_arm_keeps_one_row_and_one_resume() {
    for (case, name) in [("wake-when", None), ("custom", Some("my-settle-wait"))] {
        let world = World::new(automation()).await;
        let flow = flow_named(&world, &format!("manual-first-{case}")).await;
        let (request, executed) =
            accept_only(&world, &flow, &format!("manual-first-{case}"), None).await;
        let execution_id = executed.result.execution_id;
        let (manual, replaced) = arm_manual(&world, world.lead_a, execution_id, name).await;
        assert!(replaced.is_empty(), "{case}: nothing to replace yet");

        assert!(
            !arm_default(&world, &request, &executed).await,
            "{case}: the default is skipped"
        );
        let rows = owned_settle_rows(&world, world.lead_a, execution_id).await;
        assert_eq!(rows.len(), 1, "{case}: one row");
        assert!(rows[0].enabled);
        assert_eq!(rows[0].id, manual.id, "{case}: the manual wake survives");
        assert_eq!(
            resumes_after_settle(&world, world.lead_a, execution_id).await,
            1,
            "{case}: exactly one resume"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_manual_settle_wake_after_the_default_replaces_it_for_one_row_and_one_resume() {
    for (case, name) in [("wake-when", None), ("custom", Some("my-settle-wait"))] {
        let world = World::new(automation()).await;
        let flow = flow_named(&world, &format!("default-first-{case}")).await;
        let (result, armed) =
            execute_with_default_wake(&world, &flow, &format!("default-first-{case}"), None).await;
        assert!(armed);
        let default = settle_wakes(&world, result.execution_id).await.remove(0);

        let (manual, replaced) = arm_manual(&world, world.lead_a, result.execution_id, name).await;
        assert_eq!(replaced, vec![default.id], "{case}: the result names it");
        let rows = owned_settle_rows(&world, world.lead_a, result.execution_id).await;
        let enabled: Vec<_> = rows.iter().filter(|row| row.enabled).collect();
        assert_eq!(enabled.len(), 1, "{case}: one enabled row");
        assert_eq!(enabled[0].id, manual.id, "{case}: the manual arm wins");
        assert_eq!(
            resumes_after_settle(&world, world.lead_a, result.execution_id).await,
            1,
            "{case}: exactly one resume"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn when_topology_execution_settled_fires_once_on_success() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let execution = started(&world, "wake-success").await;
    let wake = arm(&world, world.lead_a, execution).await.unwrap();

    // Still in flight: pending, nothing delivered.
    lane.pass(&world, wake.id).await;
    assert!(lane.launcher.deliveries().is_empty());

    settle(&world, execution, ExecutionStatus::Succeeded, None).await;
    lane.pass(&world, wake.id).await;
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "one owner wake");
    let (target, message) = &deliveries[0];
    assert_eq!(*target, world.lead_a);
    assert!(
        message.contains(&format!(
            "topology_execution_settled {execution}: satisfied\ntimed_out: false"
        )),
        "{message}"
    );
    assert!(
        message.contains(&format!("execution_id={execution} status=succeeded")),
        "{message}"
    );
    assert!(message.contains("finished_at="), "{message}");

    // Spent for good: later passes never wake again.
    lane.pass(&world, wake.id).await;
    lane.pass(&world, wake.id).await;
    assert_eq!(lane.launcher.deliveries().len(), 1);
    let row = world
        .harness
        .executor
        .store
        .lock()
        .await
        .get_scheduled_job(&wake.id)
        .unwrap()
        .unwrap();
    assert!(!row.enabled, "a delivered predicate wake is spent");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn when_topology_execution_settled_fires_on_failed_with_error() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let execution = started(&world, "wake-failed").await;
    let wake = arm(&world, world.lead_a, execution).await.unwrap();

    settle(
        &world,
        execution,
        ExecutionStatus::Failed,
        Some("node A exhausted its attempts"),
    )
    .await;
    lane.pass(&world, wake.id).await;
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1);
    let message = &deliveries[0].1;
    assert!(message.contains("status=failed"), "{message}");
    assert!(
        message.contains("error=node A exhausted its attempts"),
        "{message}"
    );
    assert!(message.contains("finished_at="), "{message}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn when_topology_execution_settled_fires_on_blocked_with_reason() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let execution = started(&world, "wake-blocked").await;
    let wake = arm(&world, world.manager, execution).await.unwrap();

    lane.pass(&world, wake.id).await;
    assert!(lane.launcher.deliveries().is_empty());

    let blocked = world.blocked_on_preserved_work(execution).await;
    lane.pass(&world, wake.id).await;
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "a block wakes the owner");
    let (target, message) = &deliveries[0];
    assert_eq!(*target, world.manager);
    assert!(message.contains("status=blocked"), "{message}");
    assert!(message.contains("blocked_reason=kind:"), "{message}");
    assert!(message.contains("node_id:A"), "{message}");
    assert!(
        message.contains(&format!("attempt_id:{}", blocked.id)),
        "{message}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn when_topology_execution_settled_refuses_an_execution_outside_scope() {
    let world = World::new(automation()).await;
    let execution = started(&world, "wake-scope").await;

    // The requester and a manager that can read the Epic may arm it.
    assert!(arm(&world, world.lead_a, execution).await.is_ok());
    assert!(arm(&world, world.manager, execution).await.is_ok());

    // Another Epic's lead, an ordinary worker and an unknown id are all the
    // one `wake_when_execution_not_found`.
    for (owner, id) in [
        (world.lead_b, execution),
        (world.worker, execution),
        (world.lead_a, Uuid::new_v4()),
    ] {
        let error = arm(&world, owner, id).await.expect_err("refused");
        assert_eq!(
            error.to_string().rsplit(": ").next().unwrap(),
            "wake_when_execution_not_found",
            "{owner} {id}: {error}"
        );
    }
}

// ─── #2304 slice 4 (#2578): settlement, closure and recovery composed ──────
//
// The bundled starters run in the provider-free harness against the real
// durable stores: the merge queue's own entry (claimed, then settled through
// `settle_rolling_queue_entry`, so the "does a live execution record this
// outcome" rule is the production one), the Issue events and the scheduler's
// fast lane. Only the sessions, the review service and the queue-to-land
// mirror are the harness's. Every wait is a state signal (a row written, a
// finished-at moved back), never a wall-clock sleep.

/// Every wake the merge queue itself wrote (`merge-queue-*`).
async fn queue_wakes(world: &World) -> Vec<ScheduledJob> {
    let store = world.harness.executor.store.lock().await;
    store
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .filter(|job| job.name.starts_with("merge-queue-"))
        .collect()
}

async fn queue_entry_count(world: &World) -> i64 {
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .query_row("SELECT COUNT(*) FROM rolling_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// Admit one source into the real queue under `key`, waking `wake` when it
/// settles, and claim it (`gating`), as the lander does before its gate.
async fn gating_entry(
    world: &World,
    key: &str,
    commit: &str,
    source_session: Uuid,
    wake: Uuid,
) -> Uuid {
    let store = world.harness.executor.store.lock().await;
    let (entry, _) = store
        .enqueue_rolling_queue_source_for(
            &NewQueueEntry {
                project_id: Some(world.project),
                repo_path: world.harness.repo.display().to_string(),
                source_commit: commit.to_owned(),
                source_session_id: source_session,
                owner_epic_id: Some(world.epic_a),
                binding: RollingQueueBinding::Unbound,
                work_key: None,
                migration_version: None,
                hot_files: Vec::new(),
                test_filters: vec!["rsid=topology::".into()],
                fixes_tests: Vec::new(),
                idempotency_key: key.to_owned(),
            },
            Some(wake),
            chrono::Utc::now(),
        )
        .unwrap();
    let claimed = store
        .claim_next_rolling_queue_entry(chrono::Utc::now())
        .unwrap()
        .expect("the entry is claimable");
    assert_eq!(claimed.entry.id, entry.id);
    entry.id
}

/// [`gating_entry`] for the land node's own replay key and accepted source.
async fn gating_land_entry(world: &World, land: &rows::AttemptRow, wake: Uuid) -> Uuid {
    assert!(land.dedup_key.starts_with("topology.node:"), "{land:?}");
    let (_, request) = world
        .harness
        .land_requests()
        .into_iter()
        .find(|(_, request)| request.dedup_key == land.dedup_key)
        .expect("the land node asked the queue");
    gating_entry(
        world,
        &land.dedup_key,
        &request.source_commit,
        request.author_session_id,
        wake,
    )
    .await
}

/// Settle the real queue entry and mirror its state into the land node's
/// queue gateway, as the daemon's `land_status` reads it.
async fn settle_entry(
    world: &World,
    land: &rows::AttemptRow,
    entry: Uuid,
    state: RollingQueueEntryState,
    outcome: RollingQueueOutcome,
) {
    let store = world.harness.executor.store.lock().await;
    store
        .settle_rolling_queue_entry(entry, state, &outcome, chrono::Utc::now())
        .unwrap()
        .expect("the gating entry settles");
    let status = match state {
        RollingQueueEntryState::Published => LandStatus::Published {
            landed_sha: outcome.landed_sha,
        },
        other => LandStatus::Refused {
            state: other.as_str().to_owned(),
            reason: outcome.refusal.unwrap_or_else(|| other.as_str().to_owned()),
        },
    };
    world
        .harness
        .set_land_state(land.land_entry_id.unwrap(), status);
}

async fn publish_entry(world: &World, land: &rows::AttemptRow, entry: Uuid, sha: &str) {
    settle_entry(
        world,
        land,
        entry,
        RollingQueueEntryState::Published,
        RollingQueueOutcome {
            landed_sha: Some(sha.to_owned()),
            ..RollingQueueOutcome::default()
        },
    )
    .await;
}

/// The retry backoff has elapsed: move every failed attempt's finish back.
async fn backdate_failures(world: &World, execution: Uuid) {
    let store = world.harness.executor.store.lock().await;
    let old = (chrono::Utc::now() - chrono::TimeDelta::minutes(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "UPDATE topology_node_attempts SET finished_at=?2 \
             WHERE execution_id=?1 AND status='failed'",
            rusqlite::params![execution.to_string(), old],
        )
        .unwrap();
}

/// Sessions that were launched and Issue writes the run made, which a replay
/// must not grow.
async fn effect_counts(
    world: &World,
    issue: &rsi_common::types::Issue,
    execution: Uuid,
) -> [usize; 5] {
    [
        world.harness.launches().len(),
        world.harness.land_requests().len(),
        usize::try_from(queue_entry_count(world).await).unwrap(),
        status_events(world, issue).await.len(),
        topology_events(world, execution, "topology_landed")
            .await
            .len(),
    ]
}

/// F-001/F-004/F-005: the bundled ordinary starter survives a restart after
/// its enqueue, a capacity hold before it and a restart between the queue's
/// publication and the Issue close. Routine publication settles into the live
/// execution (the queue writes no owner wake of its own); the close happens
/// once; the one armed settled predicate wakes the owner once.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_restarts_close_and_notify_once() {
    let mut world = World::new(pipeline_manager()).await;
    let lane = Lane::new();
    let owner = world.manager;
    let issue = open_issue(&world, "Restart, close and notify once").await;
    let execution = start_pipeline(&world, &issue, "settle-once").await;
    let wake = arm(&world, owner, execution).await.unwrap();

    // Capacity backoff: a refused launch is a delay, not a settlement.
    world.harness.hold_capacity(Some("free_space_limit"));
    for _ in 0..2 {
        assert_eq!(
            world.harness.executor.advance(execution).await.unwrap(),
            Step::Wait
        );
    }
    let held = world.harness.attempt(execution, "implement", 0, 1).await;
    assert_eq!(held.status, AttemptStatus::Reserved);
    assert!(world.harness.launches().is_empty());
    lane.pass(&world, wake.id).await;
    assert!(lane.launcher.deliveries().is_empty(), "held is not settled");
    world.harness.hold_capacity(None);

    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    let entry_id = land.land_entry_id.unwrap();

    // A restart after the enqueue adopts the same attempt and entry.
    world.harness.restart();
    world.harness.executor.advance(execution).await.unwrap();
    let land = world.harness.attempt(execution, "land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Waiting);
    assert_eq!(land.land_entry_id, Some(entry_id));
    assert_eq!(world.harness.land_requests().len(), 1);

    // The real queue gates the entry; in flight settles nothing.
    let entry = gating_land_entry(&world, &land, owner).await;
    lane.pass(&world, wake.id).await;
    assert!(lane.launcher.deliveries().is_empty());

    // Publication settles into the live execution: the queue writes no owner
    // wake, and the Issue is still held until the close.
    publish_entry(&world, &land, entry, &"a".repeat(40)).await;
    assert!(
        queue_wakes(&world).await.is_empty(),
        "a routine publication is not delivered to the owner by the queue"
    );
    world.harness.restart();
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
    lane.pass(&world, wake.id).await;
    assert!(lane.launcher.deliveries().is_empty(), "not settled yet");

    // The restarted daemon closes once and settles.
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    let land = world.harness.attempt(execution, "land", 0, 1).await;
    assert_eq!(
        status_events(&world, &issue).await,
        vec![
            format!("topology-start:{execution}"),
            format!("topology-land:{execution}:{}", land.id)
        ]
    );
    let counts = effect_counts(&world, &issue, execution).await;
    assert_eq!(
        counts,
        [1, 1, 1, 2, 1],
        "launches, requests, entries, writes, notes"
    );

    // One execution-settled delivery to the owner, spent for good.
    for _ in 0..3 {
        lane.pass(&world, wake.id).await;
    }
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "{deliveries:?}");
    assert_eq!(deliveries[0].0, owner);
    assert!(
        deliveries[0].1.contains(&format!(
            "topology_execution_settled {execution}: satisfied"
        )) && deliveries[0].1.contains("status=succeeded"),
        "{}",
        deliveries[0].1
    );
    assert!(
        !deliveries[0].1.contains("issue_close=refused"),
        "{}",
        deliveries[0].1
    );
    assert!(queue_wakes(&world).await.is_empty());

    // A further restart replays nothing.
    world.harness.restart();
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(effect_counts(&world, &issue, execution).await, counts);
    lane.pass(&world, wake.id).await;
    assert_eq!(lane.launcher.deliveries().len(), 1);
}

/// F-004: a pre-merge review that restarts while waiting and then runs out of
/// usable verdicts blocks with its reason and wakes the owner once (the wake is
/// spent). An authorized retry (replayed by key, refused on a stale version)
/// arms ONE new predicate for the current owner, which reports the new
/// settlement; the spent predicate never replays the old block. No duplicate
/// worker, entry or closure results.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn blocked_review_retry_arms_one_new_owner_wake_without_replaying_the_old() {
    let mut world = World::new(pipeline_manager()).await;
    let lane = Lane::new();
    let issue = open_issue(&world, "Blocked review then retry").await;
    let execution = start_starter(
        &world,
        starter_named(ISSUE_PIPELINE_REVIEWED),
        &issue,
        "blocked-review",
    )
    .await;
    let first = arm(&world, world.manager, execution).await.unwrap();

    // Restart while the review waits: adopted, not re-requested.
    deliver_verified_at(&world, execution, "implement", 0).await;
    let waiting = waiting_review(&world, execution, "review", 0).await;
    world.harness.restart();
    let after = waiting_review(&world, execution, "review", 0).await;
    assert_eq!(after.review_assignment_id, waiting.review_assignment_id);
    assert_eq!(world.harness.review_requests().len(), 1);
    lane.pass(&world, first.id).await;
    assert!(lane.launcher.deliveries().is_empty());

    // Two unusable verdicts with the retry backoff elapsed: blocked.
    world.harness.script_reviews([
        ReviewScript::Unsettled("reviewer lost"),
        ReviewScript::Unsettled("reviewer lost again"),
    ]);
    for _ in 0..12 {
        world.harness.executor.advance(execution).await.unwrap();
        backdate_failures(&world, execution).await;
        if world.harness.status(execution).await == ExecutionStatus::Blocked {
            break;
        }
    }
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Blocked
    );
    let blocked = world
        .harness
        .attempts(execution)
        .await
        .into_iter()
        .rfind(|attempt| attempt.node_id == "review")
        .unwrap();
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(
        blocked.failure_class.as_deref(),
        Some(rows::failure::REVIEW_UNSETTLED)
    );
    assert!(world.harness.land_requests().is_empty(), "nothing enqueued");
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );

    // The block reaches the owner once, with its reason; the wake is spent.
    for _ in 0..3 {
        lane.pass(&world, first.id).await;
    }
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "{deliveries:?}");
    assert_eq!(deliveries[0].0, world.manager);
    assert!(
        deliveries[0].1.contains("status=blocked"),
        "{}",
        deliveries[0].1
    );
    assert!(
        deliveries[0]
            .1
            .contains(&format!("node_id:review attempt_id:{}", blocked.id)),
        "{}",
        deliveries[0].1
    );

    // The current owner (an Epic lead that may retry a review) arms one new
    // predicate and retries. A stale version is refused by the CAS first.
    let owner = world.lead_a;
    let version = world.harness.row_version(execution).await;
    let params = |key: &str, expected_row_version: i64| ResolveTopologyAttemptParams {
        execution_id: execution,
        attempt_id: blocked.id,
        action: TopologyAttemptAction::Retry,
        expected_row_version,
        idempotency_key: key.into(),
        confirm_preserved_commit: None,
    };
    let stale = agent::resolve(
        &world.harness.executor,
        owner,
        &params("stale", version - 1),
    )
    .await
    .unwrap_err();
    assert_eq!(error_code(&stale), "stale_row_version");
    world.harness.script_reviews([ReviewScript::Accept]);
    let retried = agent::resolve(&world.harness.executor, owner, &params("retry-1", version))
        .await
        .unwrap();
    assert_eq!(retried.attempt.resolution.as_deref(), Some("retried"));
    let second = arm(&world, owner, execution).await.unwrap();
    assert_ne!(first.id, second.id);
    lane.pass(&world, second.id).await;
    assert_eq!(
        lane.launcher.deliveries().len(),
        1,
        "running again: pending"
    );

    // The retried review accepts; the land takes the real queue and publishes.
    let land = land_waiting_on(&world, execution, "land").await;
    let entry = gating_land_entry(&world, &land, world.manager).await;
    publish_entry(&world, &land, entry, &"b".repeat(40)).await;
    assert!(queue_wakes(&world).await.is_empty());
    world.harness.restart();
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);

    // The new predicate reports the new settlement once; the old, spent one
    // never replays its block.
    for _ in 0..3 {
        lane.pass(&world, second.id).await;
        lane.pass(&world, first.id).await;
    }
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 2, "{deliveries:?}");
    assert_eq!(deliveries[1].0, owner);
    assert!(
        deliveries[1].1.contains("status=succeeded"),
        "{}",
        deliveries[1].1
    );
    assert!(
        !deliveries[1].1.contains("status=blocked"),
        "{}",
        deliveries[1].1
    );

    // Replaying the retry by key changes nothing: no new attempt, review,
    // worker, queue entry or closure.
    let counts = effect_counts(&world, &issue, execution).await;
    assert_eq!(
        counts,
        [1, 1, 1, 2, 1],
        "launches, requests, entries, writes, notes"
    );
    let attempts = world.harness.attempts(execution).await.len();
    let reviews = world.harness.review_requests().len();
    agent::resolve(&world.harness.executor, owner, &params("retry-1", version))
        .await
        .expect("a replay by key answers");
    assert_eq!(world.harness.attempts(execution).await.len(), attempts);
    assert_eq!(world.harness.review_requests().len(), reviews);
    assert_eq!(
        reviews,
        world
            .harness
            .attempts(execution)
            .await
            .iter()
            .filter(|attempt| attempt.node_id == "review")
            .count(),
        "one review request per review attempt"
    );
    assert_eq!(effect_counts(&world, &issue, execution).await, counts);
    assert_eq!(lane.launcher.deliveries().len(), 2);
}

/// F-005: the routine-queue suppression is only for a live execution. A land
/// entry that settles after its execution blocked still reaches the CURRENT
/// owner (the rotated successor of the enqueuing seat), and a hand-driven
/// entry keeps its own notice.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn late_queue_outcome_after_a_block_and_hand_entries_still_wake_the_current_owner() {
    let world = World::new(pipeline_manager()).await;
    let lane = Lane::new();
    let issue = open_issue(&world, "Late outcome after a block").await;
    let execution = start_pipeline(&world, &issue, "late-outcome").await;
    let wake = arm(&world, world.manager, execution).await.unwrap();
    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    let entry = gating_land_entry(&world, &land, world.manager).await;

    // The execution blocks while the entry gates; its settled wake fires once.
    {
        let store = world.harness.executor.store.lock().await;
        rows::transition_execution(
            &store,
            execution,
            &[ExecutionStatus::Accepted, ExecutionStatus::Running],
            ExecutionStatus::Blocked,
            None,
            None,
            Some(&serde_json::json!({"kind": "preserved_work", "node_id": "land"})),
        )
        .unwrap()
        .expect("blocked");
    }
    lane.pass(&world, wake.id).await;
    assert_eq!(lane.launcher.deliveries().len(), 1);
    assert!(lane.launcher.deliveries()[0].1.contains("status=blocked"));

    // The manager seat rotates: the old seat is archived, its only
    // continuation is live.
    let successor = Uuid::new_v4();
    {
        let store = world.harness.executor.store.lock().await;
        let mut row =
            crate::session::agent_verbs::tests::test_session(successor, world.harness.repo.clone());
        row.project_id = Some(world.project);
        row.continued_from = Some(world.manager);
        row.status = SessionStatus::Running;
        store.insert_session(&row).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Archived' WHERE id=?1",
                [world.manager.to_string()],
            )
            .unwrap();
    }

    // The late outcome is not suppressed: the blocked execution cannot carry
    // it, so the queue wakes the current owner, once.
    publish_entry(&world, &land, entry, &"c".repeat(40)).await;
    let wakes = queue_wakes(&world).await;
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    assert_eq!(wakes[0].wake_session_id, Some(successor));
    assert!(
        wakes[0].message.contains(&"c".repeat(40)),
        "{}",
        wakes[0].message
    );

    // A hand-driven entry (no topology attempt owns its key) keeps its notice.
    let hand = gating_entry(
        &world,
        "hand-driven-2578",
        &"d".repeat(40),
        world.worker,
        world.manager,
    )
    .await;
    {
        let store = world.harness.executor.store.lock().await;
        store
            .settle_rolling_queue_entry(
                hand,
                RollingQueueEntryState::Published,
                &RollingQueueOutcome {
                    landed_sha: Some("d".repeat(40)),
                    ..RollingQueueOutcome::default()
                },
                chrono::Utc::now(),
            )
            .unwrap()
            .expect("settles");
    }
    let wakes = queue_wakes(&world).await;
    assert_eq!(wakes.len(), 2, "{wakes:?}");
    assert!(
        wakes
            .iter()
            .all(|wake| wake.wake_session_id == Some(successor))
    );
    // The spent settled predicate did not fire again.
    lane.pass(&world, wake.id).await;
    assert_eq!(lane.launcher.deliveries().len(), 1);
}

/// F-001/F-005: a refused Issue close is part of the owner's result. The queue
/// published and the execution succeeded, yet the Issue stays open; the one
/// settled wake names the refusal instead of reporting a bare success.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn refused_issue_close_is_named_in_the_owner_wake_though_the_queue_published() {
    // The manager may run topologies but holds no Issue authority.
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    let lane = Lane::new();
    let issue = open_issue(&world, "Owner cannot close").await;
    let execution = start_pipeline(&world, &issue, "close-refused").await;
    let wake = arm(&world, world.manager, execution).await.unwrap();
    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    let entry = gating_land_entry(&world, &land, world.manager).await;
    publish_entry(&world, &land, entry, &"e".repeat(40)).await;
    assert!(queue_wakes(&world).await.is_empty());
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);

    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Open);
    lane.pass(&world, wake.id).await;
    lane.pass(&world, wake.id).await;
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "{deliveries:?}");
    assert!(
        deliveries[0].1.contains("status=succeeded"),
        "{}",
        deliveries[0].1
    );
    assert!(
        deliveries[0]
            .1
            .contains("issue_close=refused:agent_issue_authority_denied"),
        "{}",
        deliveries[0].1
    );
}

/// One attempt row of `execution`, written directly (the settled history a
/// post-review run leaves behind).
#[allow(clippy::too_many_arguments)]
fn attempt_row(
    store: &crate::store::Store,
    execution: Uuid,
    session: Uuid,
    node: (&str, &str),
    position: (u32, u32),
    status: &str,
    failure_class: Option<&str>,
    output: Option<serde_json::Value>,
    land_entry: Option<Uuid>,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,\
                node_kind,status,dedup_key,session_id,base_commit,input_json,output_json,\
                failure_class,integrate_action_id,created_at,updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'{}',?11,?12,?13,?14,?14)",
            rusqlite::params![
                id.to_string(),
                execution.to_string(),
                node.0,
                position.0,
                position.1,
                node.1,
                status,
                format!("dedup-{id}"),
                session.to_string(),
                "a".repeat(40),
                output.map(|output| output.to_string()),
                failure_class,
                land_entry.map(|entry| entry.to_string()),
                now,
            ],
        )
        .expect("attempt row");
    id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn when_topology_execution_settled_carries_the_outcome_digest() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let execution = started(&world, "wake-outcome").await;
    let landed = "b".repeat(40);
    let entry = Uuid::new_v4();
    let (input_issue, followup) = {
        let store = world.harness.executor.store.lock().await;
        let new = |title: &str, by: Option<Uuid>| rsi_common::types::NewIssue {
            project_id: world.project,
            title: title.into(),
            body: String::new(),
            priority: None,
            labels: Vec::new(),
            created_by_session_id: by,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        let input_issue = store.create_issue(&new("the input issue", None)).unwrap();
        let followup = store
            .create_issue(&new("kaizen: the run found a gap", Some(world.worker)))
            .unwrap();
        // An Issue the run did not file is not a followup.
        store.create_issue(&new("unrelated", None)).unwrap();
        let now = chrono::Utc::now();
        store
            .conn
            .execute(
                "UPDATE topology_executions SET input_json=?2,project_id=?3 WHERE id=?1",
                rusqlite::params![
                    execution.to_string(),
                    serde_json::json!({"issue": {
                        "display_number": input_issue.display_number,
                        "title": input_issue.title,
                        "body": "",
                        "project_id": world.project,
                    }})
                    .to_string(),
                    world.project.to_string(),
                ],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO rolling_queue_entries(id, project_id, repo_path, source_commit, \
                    source_session_id, binding, hot_files_json, test_filters_json, state, \
                    idempotency_key, enqueued_at, finished_at, row_version) \
                 VALUES (?1,?2,'/tmp/repo',?3,?4,'unbound','[]','[]','published','k',?5,?6,1)",
                rusqlite::params![
                    entry.to_string(),
                    world.project.to_string(),
                    "c".repeat(40),
                    world.worker.to_string(),
                    (now - chrono::Duration::seconds(321))
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )
            .unwrap();
        let attempt = |node, kind, position, status, class, output, entry| {
            attempt_row(
                &store,
                execution,
                world.worker,
                (node, kind),
                position,
                status,
                class,
                output,
                entry,
            )
        };
        attempt(
            "implement",
            "session",
            (0, 1),
            "succeeded",
            None,
            None,
            None,
        );
        // The first review attempt gave no usable verdict; the retry approved.
        attempt(
            "review",
            "review",
            (0, 1),
            "blocked",
            Some("review_unsettled"),
            None,
            None,
        );
        attempt(
            "review",
            "review",
            (0, 2),
            "succeeded",
            None,
            Some(serde_json::json!({"fields": {
                "verdict": "accepted", "reviewer_model": "gpt-6-astra", "round": 1,
            }})),
            None,
        );
        attempt(
            "land",
            "land",
            (0, 1),
            "succeeded",
            None,
            Some(serde_json::json!({"fields": {
                "state": "published", "landed_sha": landed, "entry_id": entry,
            }})),
            Some(entry),
        );
        (input_issue, followup)
    };
    settle(&world, execution, ExecutionStatus::Succeeded, None).await;
    let wake = arm(&world, world.lead_a, execution).await.unwrap();
    lane.pass(&world, wake.id).await;

    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1);
    let message = &deliveries[0].1;
    let digest = message.split("outcome:").nth(1).expect(message);
    for expected in [
        format!("issue #{} ", input_issue.display_number),
        "the input issue".into(),
        format!("land land: published sha={landed} gate_wall=321s"),
        format!("entry={entry}"),
        "review review: approved on attempt 2".into(),
        "reviewer=gpt-6-astra".into(),
        format!(
            "filed #{} \"kaizen: the run found a gap\"",
            followup.display_number
        ),
    ] {
        assert!(digest.contains(&expected), "{expected}: {message}");
    }
    assert!(!digest.contains("unrelated"), "{message}");
    // Nothing failed: no attempt ids, and the digest is bounded.
    assert!(!digest.contains("attempt_id"), "{message}");
    assert!(
        digest.len() <= crate::topology::outcome::OUTCOME_WAKE_BYTES,
        "{}",
        digest.len()
    );

    // The same digest is on `AgentTopologyGetExecution`.
    let result = world
        .get(world.lead_a, execution)
        .await
        .expect("get execution");
    let outcome = result.execution.outcome.expect("settled outcome");
    assert_eq!(outcome.reviews[0].attempts, 2);
    assert_eq!(outcome.reviews[0].verdict, "approved");
    assert_eq!(
        outcome.lands[0].landed_sha.as_deref(),
        Some(landed.as_str())
    );
    assert_eq!(outcome.lands[0].gate_wall_secs, Some(321));
    assert_eq!(outcome.followups.len(), 1);
    assert!(outcome.failures.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn when_topology_execution_settled_digest_names_a_failed_node_with_its_attempt() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let execution = started(&world, "wake-outcome-failed").await;
    let attempt = {
        let store = world.harness.executor.store.lock().await;
        attempt_row(
            &store,
            execution,
            world.worker,
            ("implement", "session"),
            (0, 1),
            "blocked",
            Some("preserved_work"),
            None,
            None,
        )
    };
    settle(&world, execution, ExecutionStatus::Failed, Some("blocked")).await;
    let wake = arm(&world, world.lead_a, execution).await.unwrap();
    lane.pass(&world, wake.id).await;
    let message = &lane.launcher.deliveries()[0].1;
    assert!(
        message.contains(&format!(
            "FAILED implement: blocked failure_class=preserved_work attempt_id={attempt}"
        )),
        "{message}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn outcome_digest_render_stays_within_its_bound() {
    use rsi_common::topology_agent::{AgentTopologyOutcomeFailureV1, AgentTopologyOutcomeV1};
    let outcome = AgentTopologyOutcomeV1 {
        failures: (0..20)
            .map(|n| AgentTopologyOutcomeFailureV1 {
                node_id: format!("node-{n}"),
                status: "blocked".into(),
                failure_class: Some("preserved_work".into()),
                attempt_id: Uuid::new_v4(),
            })
            .collect(),
        ..AgentTopologyOutcomeV1::default()
    };
    let text =
        crate::topology::outcome::render(&outcome, crate::topology::outcome::OUTCOME_WAKE_BYTES);
    assert!(
        text.len() <= crate::topology::outcome::OUTCOME_WAKE_BYTES,
        "{}",
        text.len()
    );
    assert!(
        text.contains("more; read AgentTopologyGetExecution"),
        "{text}"
    );
}

/// Insert `count` enabled predicate wakes for `owner` straight into the store
/// (the store enforces the caps; scope checks live in the scheduler layer).
async fn fill_wakes(
    world: &World,
    owner: Uuid,
    count: usize,
    predicate: impl Fn() -> WakePredicate,
) {
    let store = world.harness.executor.store.lock().await;
    for _ in 0..count {
        let (job, state) = build_wake_when_job(WakeWhenRequest {
            message: "filler".into(),
            name: None,
            predicate: predicate(),
            timeout_seconds: None,
            working_dir: world.harness.repo.clone(),
            provider: None,
            model: None,
            project_id: Some(world.project),
            origin_session_id: owner,
        })
        .expect("builds");
        store
            .insert_wake_when(&job, &state, false)
            .expect("under its cap");
    }
}

fn sha_wait() -> WakePredicate {
    WakePredicate {
        sha_on_rolling: Some("a".repeat(40)),
        ..WakePredicate::default()
    }
}

/// #3019: a manager holding the full 16 other `when` wakes still gets the
/// default settle wake for a new execution, and vice versa.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn settle_wakes_do_not_count_against_the_other_when_wake_cap() {
    let world = World::new(automation()).await;
    fill_wakes(
        &world,
        world.lead_a,
        rsi_common::wake_predicate::WAKE_WHEN_MAX_PER_SESSION,
        sha_wait,
    )
    .await;
    let flow = flow_named(&world, "cap-other").await;
    let (request, executed) = accept_only(&world, &flow, "cap-other", None).await;
    let arm = arm_default_full(&world, &request, &executed).await;
    assert!(arm.armed, "the settle wake arms past a full other-wake cap");
    assert_eq!(arm.refusal, None);
    assert_eq!(
        settle_wakes(&world, executed.result.execution_id)
            .await
            .len(),
        1
    );

    // The other direction: many settle wakes leave room for a non-settle wake.
    fill_wakes(&world, world.lead_a, 20, || settled(Uuid::new_v4())).await;
    let store = world.harness.executor.store.lock().await;
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "one more".into(),
        name: None,
        predicate: sha_wait(),
        timeout_seconds: None,
        working_dir: world.harness.repo.clone(),
        provider: None,
        model: None,
        project_id: Some(world.project),
        origin_session_id: world.lead_a,
    })
    .expect("builds");
    let err = store
        .insert_wake_when(&job, &state, false)
        .expect_err("the 17th non-settle wake is still refused");
    assert!(err.to_string().contains("wake_when_cap_reached"), "{err}");
}

/// #3019: past the settle-wake cap the refusal reaches the requester as a typed
/// reason instead of only a daemon log line.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_refused_default_settle_wake_reports_its_reason() {
    let world = World::new(automation()).await;
    fill_wakes(
        &world,
        world.lead_a,
        rsi_common::wake_predicate::WAKE_WHEN_MAX_SETTLE_PER_SESSION,
        || settled(Uuid::new_v4()),
    )
    .await;
    let flow = flow_named(&world, "cap-settle").await;
    let (request, executed) = accept_only(&world, &flow, "cap-settle", None).await;
    let arm = arm_default_full(&world, &request, &executed).await;
    assert!(!arm.armed);
    assert_eq!(arm.refusal.as_deref(), Some("wake_when_cap_reached"));
    assert!(
        settle_wakes(&world, executed.result.execution_id)
            .await
            .is_empty()
    );
}

/// What a seeded settled run left behind (#3387): the input Issue's state, the
/// review verdict, a filed follow-up and a refused Issue close.
struct SeededRun {
    issue_closed: bool,
    review_verdict: &'static str,
    followup: bool,
    close_refused: bool,
}

impl SeededRun {
    const CLEAN: Self = Self {
        issue_closed: true,
        review_verdict: "accepted",
        followup: false,
        close_refused: false,
    };
}

/// Give `execution` the durable rows of a run that landed `landed` for a new
/// input Issue, shaped by `run`. Returns the input Issue's display number.
async fn seed_run(world: &World, execution: Uuid, run: &SeededRun, landed: &str) -> i64 {
    let store = world.harness.executor.store.lock().await;
    let new = |title: &str, by: Option<Uuid>| rsi_common::types::NewIssue {
        project_id: world.project,
        title: title.into(),
        body: String::new(),
        priority: None,
        labels: Vec::new(),
        created_by_session_id: by,
        assignee: None,
        idea_id: None,
        source_event_id: None,
        source_finding_ref: None,
    };
    let issue = store.create_issue(&new("the input issue", None)).unwrap();
    if run.issue_closed {
        store
            .update_issue_status(issue.id, IssueStatus::Closed)
            .unwrap();
    }
    if run.followup {
        store
            .create_issue(&new("kaizen: the run found a gap", Some(world.worker)))
            .unwrap();
    }
    store
        .conn
        .execute(
            "UPDATE topology_executions SET input_json=?2,project_id=?3 WHERE id=?1",
            rusqlite::params![
                execution.to_string(),
                serde_json::json!({"issue": {
                    "display_number": issue.display_number,
                    "title": issue.title,
                    "body": "",
                    "project_id": world.project,
                }})
                .to_string(),
                world.project.to_string(),
            ],
        )
        .unwrap();
    attempt_row(
        &store,
        execution,
        world.worker,
        ("review", "review"),
        (0, 1),
        "succeeded",
        None,
        Some(serde_json::json!({"fields": {
            "verdict": run.review_verdict, "reviewer_model": "gpt-6-astra", "round": 1,
        }})),
        None,
    );
    let mut fields = serde_json::json!({"state": "published", "landed_sha": landed});
    if run.close_refused {
        fields["issue_close"] =
            serde_json::json!({"status": "refused", "code": "agent_issue_authority_denied"});
    }
    attempt_row(
        &store,
        execution,
        world.worker,
        ("land", "land"),
        (0, 1),
        "succeeded",
        None,
        Some(serde_json::json!({ "fields": fields })),
        None,
    );
    i64::from(issue.display_number)
}

/// Accept a fresh execution with the daemon's default settle wake, seed its
/// run and settle it. Returns the execution and its one settle wake.
async fn default_wake_run(
    world: &World,
    name: &str,
    run: &SeededRun,
    landed: &str,
    status: ExecutionStatus,
) -> (Uuid, Uuid) {
    let flow = flow_named(world, name).await;
    let (result, armed) = execute_with_default_wake(world, &flow, name, None).await;
    assert!(armed, "the default arms the requester's settle wake");
    seed_run(world, result.execution_id, run, landed).await;
    settle(world, result.execution_id, status, None).await;
    let wakes = settle_wakes(world, result.execution_id).await;
    assert_eq!(wakes.len(), 1);
    (result.execution_id, wakes[0].id)
}

/// #3387: a clean success (execution succeeded, Issue Closed, review approved,
/// no follow-up, no refused close) opens no owner turn. It waits in the
/// owner's digest and rides on the owner's next wake, which retires it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_clean_success_opens_no_turn_and_rides_the_next_wake_3387() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let landed = "a".repeat(40);
    let (clean, clean_wake) = default_wake_run(
        &world,
        "quiet-clean",
        &SeededRun::CLEAN,
        &landed,
        ExecutionStatus::Succeeded,
    )
    .await;
    lane.pass(&world, clean_wake).await;
    lane.pass(&world, clean_wake).await;
    assert!(
        lane.launcher.deliveries().is_empty(),
        "a clean success opens no owner turn: {:?}",
        lane.launcher.deliveries()
    );
    {
        let store = world.harness.executor.store.lock().await;
        let held = store
            .quiet_settle_state(clean_wake)
            .unwrap()
            .and_then(|record| record.held)
            .expect("the clean success is held in the digest");
        assert!(held.line.contains(&clean.to_string()), "{}", held.line);
        assert!(held.line.contains(&landed), "{}", held.line);
        assert_eq!(
            store
                .held_quiet_settles(world.lead_a, Uuid::new_v4())
                .unwrap()
                .len(),
            1
        );
    }
    assert_eq!(settle_wakes(&world, clean).await.len(), 1, "still armed");

    // The owner's next wake (a failure settles at once) carries the digest.
    let (failed, failed_wake) = default_wake_run(
        &world,
        "quiet-then-failed",
        &SeededRun::CLEAN,
        &landed,
        ExecutionStatus::Failed,
    )
    .await;
    lane.pass(&world, failed_wake).await;
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "{deliveries:?}");
    let message = &deliveries[0].1;
    assert!(
        message.contains(&format!("execution_id={failed} status=failed")),
        "{message}"
    );
    assert!(
        message.contains("Clean topology successes since your last wake (1"),
        "{message}"
    );
    assert!(
        message.contains(&format!("execution {clean} succeeded")),
        "{message}"
    );
    assert!(
        settle_wakes(&world, clean).await.is_empty(),
        "digest drained"
    );
    lane.pass(&world, clean_wake).await;
    assert_eq!(lane.launcher.deliveries().len(), 1, "never delivered twice");
}

/// #3387: every outcome that is not a clean success still wakes the owner at
/// once: failed, cancelled, blocked, and a `succeeded` run whose Issue stayed
/// open (refused close), asked for changes, or filed a follow-up.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn every_non_clean_outcome_still_wakes_the_owner_at_once_3387() {
    let world = World::new(automation()).await;
    let landed = "b".repeat(40);
    let cases: [(&str, SeededRun, ExecutionStatus); 7] = [
        ("failed", SeededRun::CLEAN, ExecutionStatus::Failed),
        ("cancelled", SeededRun::CLEAN, ExecutionStatus::Cancelled),
        ("blocked", SeededRun::CLEAN, ExecutionStatus::Blocked),
        (
            "issue-open",
            SeededRun {
                issue_closed: false,
                ..SeededRun::CLEAN
            },
            ExecutionStatus::Succeeded,
        ),
        (
            "close-refused",
            SeededRun {
                issue_closed: false,
                close_refused: true,
                ..SeededRun::CLEAN
            },
            ExecutionStatus::Succeeded,
        ),
        (
            "changes-requested",
            SeededRun {
                review_verdict: "changes_requested",
                ..SeededRun::CLEAN
            },
            ExecutionStatus::Succeeded,
        ),
        (
            "followup",
            SeededRun {
                followup: true,
                ..SeededRun::CLEAN
            },
            ExecutionStatus::Succeeded,
        ),
    ];
    for (name, run, status) in cases {
        let lane = Lane::new();
        let (execution, wake) =
            default_wake_run(&world, &format!("loud-{name}"), &run, &landed, status).await;
        lane.pass(&world, wake).await;
        let deliveries = lane.launcher.deliveries();
        assert_eq!(deliveries.len(), 1, "{name} wakes at once: {deliveries:?}");
        assert!(
            deliveries[0]
                .1
                .contains(&format!("execution_id={execution} ")),
            "{name}: {}",
            deliveries[0].1
        );
        assert!(
            !deliveries[0].1.contains("Clean topology successes"),
            "{name}: {}",
            deliveries[0].1
        );
    }
}

/// #3387: a settle wake the owner armed itself keeps its immediate turn even
/// for a clean success.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_owner_armed_settle_wake_keeps_its_turn_on_a_clean_success_3387() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let execution = started(&world, "quiet-manual").await;
    seed_run(&world, execution, &SeededRun::CLEAN, &"c".repeat(40)).await;
    let (manual, _) = arm_manual(&world, world.lead_a, execution, None).await;
    settle(&world, execution, ExecutionStatus::Succeeded, None).await;
    lane.pass(&world, manual.id).await;
    assert_eq!(lane.launcher.deliveries().len(), 1);
}

/// #3387: a held success is not lost when the owner has no other wake: once it
/// has waited the batch deadline it is delivered alone, with the rest of the
/// digest, as one wake.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_held_success_is_delivered_as_one_batch_after_the_deadline_3387() {
    let world = World::new(automation()).await;
    let lane = Lane::new();
    let landed = "d".repeat(40);
    let (first, first_wake) = default_wake_run(
        &world,
        "batch-first",
        &SeededRun::CLEAN,
        &landed,
        ExecutionStatus::Succeeded,
    )
    .await;
    let (second, second_wake) = default_wake_run(
        &world,
        "batch-second",
        &SeededRun::CLEAN,
        &landed,
        ExecutionStatus::Succeeded,
    )
    .await;
    lane.pass(&world, first_wake).await;
    lane.pass(&world, second_wake).await;
    assert!(lane.launcher.deliveries().is_empty());

    // The first success has now waited past the batch deadline.
    {
        let store = world.harness.executor.store.lock().await;
        let mut held = store
            .quiet_settle_state(first_wake)
            .unwrap()
            .and_then(|record| record.held)
            .expect("held");
        held.at = chrono::Utc::now()
            - chrono::Duration::seconds(rsi_common::wake_predicate::QUIET_SETTLE_MAX_HOLD_SECS + 5);
        store
            .hold_quiet_settle(first_wake, &held, chrono::Utc::now())
            .unwrap();
    }
    lane.pass(&world, first_wake).await;
    let deliveries = lane.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "one batched wake: {deliveries:?}");
    let message = &deliveries[0].1;
    assert!(
        message.contains(&format!("execution_id={first} status=succeeded")),
        "{message}"
    );
    assert!(
        message.contains(&format!("execution {second} succeeded")),
        "the batch carries the other held success: {message}"
    );
    assert!(
        settle_wakes(&world, second).await.is_empty(),
        "retired with the batch"
    );
    lane.pass(&world, second_wake).await;
    assert_eq!(
        lane.launcher.deliveries().len(),
        1,
        "nothing delivered twice"
    );
}
