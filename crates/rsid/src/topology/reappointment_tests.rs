//! #3274: an operator re-appointment of the project manager seat. The new seat
//! adopts the executions the displaced seat requested while their Epic stays in
//! scope (the next agent node launches); a scope that drops the Epic still
//! blocks them `manager_scope_changed`; a launch refused by live policy is
//! retryable once the cause clears.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::rpc::{ResolveTopologyAttemptParams, TopologyAttemptAction};
use rsi_common::types::{SessionKind, SessionStatus};
use uuid::Uuid;

use super::{
    AttemptStatus, ExecutionStatus, ManagerPolicyV2, ReviewScript, Step, World, agent, automation,
    error_code, luna, portfolio_run_with_a_done, review_definition, review_world_policy, rows,
    sonnet, upsert_request,
};

/// The operator seats a fresh, parentless session as the project manager with
/// `epics` in scope (the scope version bumps and the grant is revoked until
/// re-saved by the caller). Returns the new seat.
pub(super) async fn reappoint(world: &World, epics: Vec<Uuid>) -> Uuid {
    let seat = Uuid::new_v4();
    let store = world.harness.executor.store.lock().await;
    let mut row =
        crate::session::agent_verbs::tests::test_session(seat, world.harness.repo.clone());
    row.project_id = Some(world.project);
    row.session_kind = SessionKind::Standard;
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    let scope = store.get_harness_manager(world.project).unwrap().unwrap();
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: vec![],
            project_id: world.project,
            session_id: seat,
            epic_ids: Some(epics),
            expected_row_version: scope.row_version,
        })
        .unwrap();
    seat
}

async fn execution_scope(world: &World, run: Uuid) -> (Option<i64>, Option<Uuid>) {
    let store = world.harness.executor.store.lock().await;
    let row = rows::load_execution(&store, run).unwrap().unwrap();
    (row.scope_version, row.requested_by_session_id)
}

async fn adopted_events(world: &World, run: Uuid) -> Vec<serde_json::Value> {
    super::issue_close_tests::topology_events(world, run, "execution_adopted").await
}

async fn run_until_done_or_blocked(world: &World, run: Uuid) {
    for _ in 0..6 {
        if world.harness.executor.advance(run).await.unwrap() == Step::Done {
            break;
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn reappointed_seat_adopts_an_in_scope_execution_and_its_next_node_launches() {
    let world = World::new(automation()).await;
    let run = portfolio_run_with_a_done(&world, world.manager).await;
    let (stamped, _) = execution_scope(&world, run).await;

    let seat = reappoint(&world, vec![world.epic_a, world.epic_b]).await;
    world.grant(automation()).await;
    let scope = {
        let store = world.harness.executor.store.lock().await;
        store.get_harness_manager(world.project).unwrap().unwrap()
    };
    assert_eq!(scope.current_session_id, Some(seat));
    assert_ne!(
        stamped,
        Some(scope.row_version),
        "the appointment bumped the scope"
    );

    // Re-stamped onto the new scope; the requester stays the immutable provenance.
    assert_eq!(
        execution_scope(&world, run).await,
        (Some(scope.row_version), Some(world.manager))
    );
    let events = adopted_events(&world, run).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let detail = &events[0]["detail"];
    assert_eq!(detail["previous_seat"], world.manager.to_string());
    assert_eq!(detail["seat"], seat.to_string());
    assert_eq!(detail["scope_version"], scope.row_version);

    // The next agent node launches under the new seat's live authority.
    run_until_done_or_blocked(&world, run).await;
    let attempts = world.harness.attempts(run).await;
    let second = attempts.iter().find(|a| a.node_id == "B").expect("B");
    assert_eq!(second.status, AttemptStatus::Running, "{attempts:?}");
    assert_eq!(world.harness.launches().len(), 2);
    // The displaced seat's execution lands under the new holder.
    let store = world.harness.executor.store.lock().await;
    let execution = rows::load_execution(&store, run).unwrap().unwrap();
    assert_eq!(agent::landing_gate(&store, &execution).unwrap(), None);
    assert_eq!(
        agent::live_owner(&store, world.manager).unwrap(),
        seat,
        "the displaced seat is represented by the appointment's current seat"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_scope_that_drops_the_execution_epic_still_blocks_it() {
    let world = World::new(automation()).await;
    let run = portfolio_run_with_a_done(&world, world.manager).await;
    let (stamped, _) = execution_scope(&world, run).await;

    // The new appointment covers only Epic B; the run belongs to Epic A.
    let _seat = reappoint(&world, vec![world.epic_b]).await;
    world.grant(automation()).await;
    assert_eq!(execution_scope(&world, run).await.0, stamped, "not adopted");
    assert!(adopted_events(&world, run).await.is_empty());

    run_until_done_or_blocked(&world, run).await;
    let attempts = world.harness.attempts(run).await;
    let blocked = attempts.iter().find(|a| a.node_id == "B").expect("B");
    assert_eq!(blocked.status, AttemptStatus::Blocked, "{attempts:?}");
    assert_eq!(blocked.error.as_deref(), Some("manager_scope_changed"));
    assert_eq!(world.harness.launches().len(), 1, "only node A launched");
    let store = world.harness.executor.store.lock().await;
    let execution = rows::load_execution(&store, run).unwrap().unwrap();
    assert!(
        agent::landing_gate(&store, &execution).unwrap().is_some(),
        "the displaced seat's execution cannot land outside the new scope"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_same_seat_scope_edit_is_not_an_adoption() {
    let world = World::new(automation()).await;
    let run = portfolio_run_with_a_done(&world, world.manager).await;
    let (stamped, _) = execution_scope(&world, run).await;
    {
        let store = world.harness.executor.store.lock().await;
        let scope = store.get_harness_manager(world.project).unwrap().unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: vec![],
                project_id: world.project,
                session_id: world.manager,
                epic_ids: Some(vec![world.epic_a]),
                expected_row_version: scope.row_version,
            })
            .unwrap();
    }
    world.grant(automation()).await;
    assert_eq!(execution_scope(&world, run).await.0, stamped);
    run_until_done_or_blocked(&world, run).await;
    let blocked = world.harness.attempt(run, "B", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(blocked.error.as_deref(), Some("manager_scope_changed"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_policy_refused_attempt_retries_once_the_cause_clears() {
    let world = World::new(automation()).await;
    let run = portfolio_run_with_a_done(&world, world.manager).await;
    world
        .grant(ManagerPolicyV2 {
            paused: true,
            ..automation()
        })
        .await;
    run_until_done_or_blocked(&world, run).await;
    let blocked = world.harness.attempt(run, "B", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(blocked.failure_class.as_deref(), Some("policy_refused"));
    assert_eq!(world.harness.status(run).await, ExecutionStatus::Blocked);

    let params = |action, key: &str, version| ResolveTopologyAttemptParams {
        execution_id: run,
        attempt_id: blocked.id,
        action,
        expected_row_version: version,
        idempotency_key: key.into(),
        confirm_preserved_commit: None,
    };
    // The cause clears (the pause lifts); resolving is allowed again. Only a
    // retry resolves it: accept has nothing to act on.
    world.grant(automation()).await;
    for action in [TopologyAttemptAction::Accept] {
        let version = world.harness.row_version(run).await;
        let error = agent::resolve(
            &world.harness.executor,
            world.manager,
            &params(action, "wrong-action", version),
        )
        .await
        .unwrap_err();
        assert_eq!(error_code(&error), "precondition_failed", "{action:?}");
    }

    // The retry launches B again.
    let version = world.harness.row_version(run).await;
    agent::resolve(
        &world.harness.executor,
        world.manager,
        &params(TopologyAttemptAction::Retry, "retry-policy", version),
    )
    .await
    .unwrap();
    run_until_done_or_blocked(&world, run).await;
    let first = world.harness.attempt(run, "B", 0, 1).await;
    assert_eq!(first.resolution.as_deref(), Some("retried"));
    let second = world.harness.attempt(run, "B", 0, 2).await;
    assert_eq!(second.status, AttemptStatus::Running, "{second:?}");
}

/// #3784: a seat change can refuse a review before its ledger Work row exists.
/// Restoring the live grant admits its first assignment, without asking for an
/// extra round of a review that has never run.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn reappointed_seat_retries_a_review_refused_before_its_first_assignment() {
    let world = World::new(review_world_policy()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("seat-review-retry", review_definition(&sonnet(), &luna())),
        )
        .await
        .unwrap();
    let run = world
        .execute(world.manager, &flow, world.epic_a, "seat-review-retry")
        .await
        .unwrap()
        .execution_id;
    world.harness.executor.advance(run).await.unwrap();
    let author = world.harness.attempt(run, "A", 0, 1).await;
    world
        .harness
        .commit_and_complete(author.session_id, "author");

    let seat = reappoint(&world, vec![world.epic_a, world.epic_b]).await;
    run_until_done_or_blocked(&world, run).await;
    let blocked = world.harness.attempt(run, "R", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(blocked.failure_class.as_deref(), Some("policy_refused"));
    assert_eq!(blocked.error.as_deref(), Some("launch_not_granted"));
    assert_eq!(blocked.review_assignment_id, None);

    world.grant(review_world_policy()).await;
    world
        .harness
        .script_extra_round(super::super::review::ExtraRound::Unavailable(
            "manager_review_work_missing".into(),
        ));
    world.harness.script_reviews([ReviewScript::Accept]);
    world
        .resolve(
            seat,
            run,
            blocked.id,
            TopologyAttemptAction::Retry,
            "seat-retry",
            None,
        )
        .await
        .unwrap();
    run_until_done_or_blocked(&world, run).await;
    let retried = world.harness.attempt(run, "R", 0, 2).await;
    assert_eq!(retried.status, AttemptStatus::Succeeded);
    assert!(retried.review_assignment_id.is_some());
    assert_eq!(world.harness.review_requests().len(), 1);
    assert!(world.harness.extra_round_asks().is_empty());
    let store = world.harness.executor.store.lock().await;
    let execution = rows::load_execution(&store, run).unwrap().unwrap();
    assert_eq!(execution.requested_by_session_id, Some(world.manager));
}
