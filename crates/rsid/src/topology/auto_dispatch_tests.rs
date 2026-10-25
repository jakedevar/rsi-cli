//! #3390 acceptance tests: the daemon dispatcher for ordinary ready Issues.
//! A child module of `agent_tests`, so it shares its `World`: the real seeded
//! starters, the real manager policy and the real `agent::execute` admission.

use std::time::Instant;

use super::starters_tests::pipeline_manager;
use super::*;
use crate::topology::auto_dispatch::{
    self, AUTO_LABEL, BUILD_HEAVY_CAP, Dispatch, Route, Skip, capacity_limit, classify,
};
use crate::topology::starters::{self, ISSUE_PIPELINE, ISSUE_PIPELINE_POST_REVIEW, STARTERS};
use rsi_common::types::{Issue, IssueStatus, NewIssue};

fn issue_row(labels: &[&str], assignee: Option<&str>, status: IssueStatus) -> Issue {
    let now = chrono::Utc::now();
    Issue {
        id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        display_number: 1,
        title: "t".into(),
        body: String::new(),
        status,
        priority: None,
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        created_by_session_id: None,
        assignee: assignee.map(str::to_owned),
        idea_id: None,
        source_event_id: None,
        source_finding_ref: None,
        created_at: now,
        updated_at: now,
        closed_at: None,
        row_version: 1,
        archived_at: None,
    }
}

fn open(labels: &[&str]) -> Issue {
    issue_row(labels, None, IssueStatus::Open)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn eligibility_requires_the_label_and_an_open_unclaimed_issue() {
    assert_eq!(classify(&open(&[AUTO_LABEL])), Ok(Route::Pipeline));
    // Case and padding of the label do not matter.
    assert_eq!(classify(&open(&[" Dispatch:Auto "])), Ok(Route::Pipeline));
    assert_eq!(classify(&open(&["bug"])), Err(Skip::NotOptedIn));
    assert_eq!(classify(&open(&[])), Err(Skip::NotOptedIn));
    // `dispatch:manual` is not the opt-in.
    assert_eq!(classify(&open(&["dispatch:manual"])), Err(Skip::NotOptedIn));
    for status in [
        IssueStatus::InProgress,
        IssueStatus::Closed,
        IssueStatus::Cancelled,
    ] {
        assert_eq!(
            classify(&issue_row(&[AUTO_LABEL], None, status)),
            Err(Skip::NotOpen)
        );
    }
    let mut archived = open(&[AUTO_LABEL]);
    archived.archived_at = Some(chrono::Utc::now());
    assert_eq!(classify(&archived), Err(Skip::Archived));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_satellite_claimed_issue_is_never_auto_dispatched() {
    for assignee in ["satellite:laptop", "satellite:mac-1", " Satellite:lap2 "] {
        assert_eq!(
            classify(&issue_row(&[AUTO_LABEL], Some(assignee), IssueStatus::Open)),
            Err(Skip::SatelliteClaimed),
            "{assignee}"
        );
    }
    // Another assignee string is not a satellite claim.
    assert_eq!(
        classify(&issue_row(&[AUTO_LABEL], Some("jake"), IssueStatus::Open)),
        Ok(Route::Pipeline)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn migration_and_credential_work_stays_with_the_manager() {
    for label in [
        "migration",
        "migrations",
        "credential",
        "credentials",
        "provider-credentials",
        "iam",
        "network-exposure",
        "Migration",
    ] {
        assert_eq!(
            classify(&open(&[AUTO_LABEL, label])),
            Err(Skip::NeedsManagerJudgement(label.to_ascii_lowercase())),
            "{label}"
        );
    }
    // Pre-merge review wins over the post-review route.
    assert!(classify(&open(&[AUTO_LABEL, "authority", "migration"])).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn authority_custody_and_queue_work_takes_the_post_review_starter() {
    for label in [
        "authority",
        "custody",
        "lander",
        "queue",
        "merge-queue",
        "rolling-queue",
        "deploy",
        "deployment",
        "agent-authority",
        "issue-authority",
    ] {
        assert_eq!(
            classify(&open(&[AUTO_LABEL, label])),
            Ok(Route::PostReview),
            "{label}"
        );
    }
    assert_eq!(Route::Pipeline.starter(), ISSUE_PIPELINE);
    assert_eq!(Route::PostReview.starter(), ISSUE_PIPELINE_POST_REVIEW);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn capacity_never_exceeds_the_build_heavy_cap_or_the_managers_own_limit() {
    assert_eq!(BUILD_HEAVY_CAP, 5);
    assert_eq!(capacity_limit(1), 1);
    assert_eq!(capacity_limit(4), 4);
    assert_eq!(capacity_limit(5), 5);
    assert_eq!(capacity_limit(32), 5);
    assert_eq!(capacity_limit(0), 0);
}

async fn labelled(world: &World, title: &str, labels: &[&str], assignee: Option<&str>) -> Issue {
    let store = world.harness.executor.store.lock().await;
    store
        .create_issue(&NewIssue {
            project_id: world.project,
            title: title.into(),
            body: "Fix it.".into(),
            priority: None,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            created_by_session_id: None,
            assignee: assignee.map(str::to_owned),
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        })
        .unwrap()
}

async fn plan(world: &World) -> Vec<Dispatch> {
    let store = world.harness.executor.store.lock().await;
    starters::seed_all(&store);
    auto_dispatch::plan_project(&store, world.project, Instant::now()).unwrap()
}

/// Accept one plan exactly as the daemon does (minus the wake and the driver,
/// which `SessionManager::agent_topology_execute` adds around the same call).
async fn accept(world: &World, plan: &Dispatch) -> Result<Uuid> {
    let mut request = plan.request.clone();
    request.base_commit = Some(world.harness.base.clone());
    agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        plan.caller,
        &request,
    )
    .await
    .map(|executed| executed.result.execution_id)
}

fn numbers(plans: &[Dispatch]) -> Vec<i64> {
    plans.iter().map(|plan| plan.issue_number).collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_labelled_ready_issue_starts_an_execution_owned_by_the_manager_seat() {
    let world = World::new(pipeline_manager()).await;
    let plain = labelled(&world, "not opted in", &["bug"], None).await;
    let issue = labelled(&world, "ordinary", &[AUTO_LABEL, "bug"], None).await;

    let plans = plan(&world).await;
    assert_eq!(numbers(&plans), vec![issue.display_number]);
    let planned = &plans[0];
    assert_eq!(planned.route, Route::Pipeline);
    assert_eq!(planned.caller, world.manager);
    assert_eq!(planned.request.topology_id, starter_id(ISSUE_PIPELINE));
    assert!(
        planned.request.epic_id == world.epic_a || planned.request.epic_id == world.epic_b,
        "the execution runs under an Epic in the seat's scope"
    );
    assert_eq!(
        planned.request.idempotency_key,
        format!("auto-dispatch:{}:{}", world.project, issue.display_number)
    );

    let execution = accept(&world, planned).await.unwrap();
    // Accepted as the manager: the seat sees it and can stop it as today.
    let seen = world.get(world.manager, execution).await.unwrap();
    assert_eq!(seen.execution.requested_by_kind, "manager");
    let version = world.harness.row_version(execution).await;
    world
        .interrupt(world.manager, execution, version, "stop")
        .await
        .unwrap();
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Cancelling
    );
    // The Issue is held (`InProgress`), so the next tick plans nothing for it
    // and never touches the one that did not opt in.
    {
        let store = world.harness.executor.store.lock().await;
        assert_eq!(
            store.get_issue(issue.id).unwrap().unwrap().status,
            IssueStatus::InProgress
        );
        assert_eq!(
            store.get_issue(plain.id).unwrap().unwrap().status,
            IssueStatus::Open
        );
    }
    assert!(plan(&world).await.is_empty());
}

fn starter_id(name: &str) -> Uuid {
    STARTERS.iter().find(|s| s.name == name).unwrap().id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn authority_work_is_routed_to_the_post_review_starter() {
    let world = World::new(pipeline_manager()).await;
    let issue = labelled(&world, "custody fix", &[AUTO_LABEL, "custody"], None).await;
    let plans = plan(&world).await;
    assert_eq!(numbers(&plans), vec![issue.display_number]);
    assert_eq!(plans[0].route, Route::PostReview);
    assert_eq!(
        plans[0].request.topology_id,
        starter_id(ISSUE_PIPELINE_POST_REVIEW)
    );
    accept(&world, &plans[0]).await.unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn ineligible_issues_are_left_to_the_manager() {
    let world = World::new(pipeline_manager()).await;
    labelled(&world, "no label", &["bug"], None).await;
    labelled(&world, "satellite", &[AUTO_LABEL], Some("satellite:laptop")).await;
    labelled(&world, "migration", &[AUTO_LABEL, "migration"], None).await;
    labelled(&world, "credentials", &[AUTO_LABEL, "credentials"], None).await;
    let blocker = labelled(&world, "blocker", &["bug"], None).await;
    let blocked = labelled(&world, "blocked", &[AUTO_LABEL], None).await;
    {
        let store = world.harness.executor.store.lock().await;
        store.add_issue_dep(blocked.id, blocker.id).unwrap();
    }
    assert!(plan(&world).await.is_empty(), "nothing is eligible");

    // Once the blocker is no longer open the Issue is ready and dispatched.
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE issues SET status='Closed' WHERE id=?1",
                [blocker.id.to_string()],
            )
            .unwrap();
    }
    assert_eq!(numbers(&plan(&world).await), vec![blocked.display_number]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn candidates_are_found_only_where_an_open_issue_carries_the_label() {
    let world = World::new(pipeline_manager()).await;
    labelled(&world, "no label", &["bug"], None).await;
    {
        let store = world.harness.executor.store.lock().await;
        assert!(
            !auto_dispatch::candidate_projects(&store)
                .unwrap()
                .contains(&world.project)
        );
    }
    labelled(&world, "opted in", &[" Dispatch:AUTO"], None).await;
    let store = world.harness.executor.store.lock().await;
    assert!(
        auto_dispatch::candidate_projects(&store)
            .unwrap()
            .contains(&world.project)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn capacity_is_respected_and_freed_when_an_execution_settles() {
    let mut limited = pipeline_manager();
    limited.max_active_sessions = 2;
    limited.max_created_sessions = 1024;
    let world = World::new(limited).await;
    for title in ["a", "b", "c", "d"] {
        labelled(&world, title, &[AUTO_LABEL], None).await;
    }
    // Two slots: the first tick starts the two oldest and no more.
    let first = plan(&world).await;
    assert_eq!(first.len(), 2);
    let mut live = Vec::new();
    for planned in &first {
        live.push(accept(&world, planned).await.unwrap());
    }
    assert!(
        plan(&world).await.is_empty(),
        "both slots are taken by live executions"
    );
    // One settles: a slot frees and exactly one more is dispatched.
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topology_executions SET status='succeeded' WHERE id=?1",
                [live[0].to_string()],
            )
            .unwrap();
    }
    let next = plan(&world).await;
    assert_eq!(next.len(), 1);
    assert!(!numbers(&first).contains(&next[0].issue_number));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_paused_or_non_execute_manager_dispatches_nothing() {
    let world = World::new(pipeline_manager()).await;
    labelled(&world, "ordinary", &[AUTO_LABEL], None).await;
    assert_eq!(plan(&world).await.len(), 1);

    let mut paused = pipeline_manager();
    paused.paused = true;
    world.grant(paused).await;
    assert!(plan(&world).await.is_empty(), "a paused manager");

    let mut status_only = pipeline_manager();
    status_only.mode = ManagerOperatingModeV2::Status;
    world.grant(status_only).await;
    assert!(
        plan(&world).await.is_empty(),
        "a manager not in execute mode"
    );

    let mut no_automation = pipeline_manager();
    no_automation.capabilities.clear();
    world.grant(no_automation).await;
    assert!(plan(&world).await.is_empty(), "no Automation capability");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_project_without_a_manager_seat_is_not_dispatched() {
    let world = World::new(pipeline_manager()).await;
    let stranger = Uuid::new_v4();
    let store = world.harness.executor.store.lock().await;
    starters::seed_all(&store);
    assert!(
        auto_dispatch::plan_project(&store, stranger, Instant::now())
            .unwrap()
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_issue_that_already_had_an_execution_is_never_dispatched_again() {
    let world = World::new(pipeline_manager()).await;
    let issue = labelled(&world, "ordinary", &[AUTO_LABEL], None).await;
    let plans = plan(&world).await;
    let execution = accept(&world, &plans[0]).await.unwrap();
    // The run fails and the Issue returns to Open with its label: a retry
    // loop is the PM's call, never the dispatcher's.
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topology_executions SET status='failed' WHERE id=?1",
                [execution.to_string()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE issues SET status='Open' WHERE id=?1",
                [issue.id.to_string()],
            )
            .unwrap();
    }
    assert!(plan(&world).await.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_issue_held_by_a_live_execution_is_not_started_twice() {
    let world = World::new(pipeline_manager()).await;
    let issue = labelled(&world, "ordinary", &[AUTO_LABEL], None).await;
    // The PM hand-launches it first; the Issue goes InProgress and then even a
    // reopened row is held by the live execution.
    execute_by_hand(&world, issue.display_number).await;
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE issues SET status='Open' WHERE id=?1",
                [issue.id.to_string()],
            )
            .unwrap();
    }
    assert!(plan(&world).await.is_empty());
}

async fn execute_by_hand(world: &World, number: i64) -> Uuid {
    let id = starter_id(ISSUE_PIPELINE);
    let digest = {
        let store = world.harness.executor.store.lock().await;
        starters::seed_all(&store);
        let text: String = store
            .conn
            .query_row(
                "SELECT definition_json FROM topologies WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        crate::topology::store::digest(&text)
    };
    agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        world.manager,
        &AgentTopologyExecuteRequestV1 {
            project_id: None,
            on_call: None,
            supersedes_execution: None,
            wake: None,
            topology_id: id,
            expected_digest: digest,
            epic_id: world.epic_a,
            inputs: serde_json::json!({"issue": number}),
            base_commit: Some(world.harness.base.clone()),
            idempotency_key: format!("hand-{number}"),
        },
    )
    .await
    .unwrap()
    .result
    .execution_id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_replayed_dispatch_request_is_deduplicated() {
    let world = World::new(pipeline_manager()).await;
    labelled(&world, "ordinary", &[AUTO_LABEL], None).await;
    let plans = plan(&world).await;
    let first = accept(&world, &plans[0]).await.unwrap();
    assert_eq!(accept(&world, &plans[0]).await.unwrap(), first);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_dispatch_the_admission_refuses_backs_off_instead_of_retrying_every_tick() {
    // A manager whose creation allowance is exhausted: admission refuses.
    let mut broke = pipeline_manager();
    broke.max_created_sessions = 0;
    let world = World::new(broke).await;
    let issue = labelled(&world, "ordinary", &[AUTO_LABEL], None).await;
    let plans = plan(&world).await;
    assert_eq!(plans.len(), 1);
    let refused = accept(&world, &plans[0]).await.unwrap_err();
    assert_eq!(error_code(&refused), "creation_limit");
    {
        let store = world.harness.executor.store.lock().await;
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM topology_executions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "a refused dispatch leaves no execution");
    }

    auto_dispatch::note_refused(world.project, issue.display_number, Instant::now());
    assert!(
        plan(&world).await.is_empty(),
        "the refused Issue is left alone for the backoff window"
    );
}

/// Seed one running execution of the project whose only node attempt is
/// `node_kind` in `status` (a `land` node waiting on the merge queue, or a
/// `session` node holding a live worker).
async fn seed_execution(world: &World, node_kind: &str, status: &str) {
    let store = world.harness.executor.store.lock().await;
    let now = "2026-10-22T00:00:00.000000000Z";
    let execution = Uuid::new_v4();
    store
        .conn
        .execute(
            "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id,definition_json,definition_digest,project_id,requested_by_kind,repo_root,base_commit,custody_plan_json,status,created_at,updated_at)
             VALUES (?1,'seed','wf','{}','d',?2,'manager','/tmp/repo','0000000000000000000000000000000000000000','{}','running',?3,?3)",
            rusqlite::params![execution.to_string(), world.project.to_string(), now],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,input_json,created_at,updated_at)
             VALUES (?1,?2,'n',0,1,?3,?4,?5,'{}',?6,?6)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                execution.to_string(),
                node_kind,
                status,
                format!("dedup-{execution}"),
                now
            ],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn executions_parked_at_land_do_not_use_the_session_cap() {
    let world = World::new(pipeline_manager()).await;
    for _ in 0..10 {
        seed_execution(&world, "land", "waiting").await;
    }
    seed_execution(&world, "session", "running").await;
    let issue = labelled(&world, "ready", &[AUTO_LABEL], None).await;
    // 11 open executions, 1 live session: the next tick starts the Issue.
    assert_eq!(numbers(&plan(&world).await), vec![issue.display_number]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn live_session_executions_still_fill_the_build_heavy_cap() {
    let world = World::new(pipeline_manager()).await;
    for _ in 0..BUILD_HEAVY_CAP {
        seed_execution(&world, "session", "running").await;
    }
    labelled(&world, "ready", &[AUTO_LABEL], None).await;
    assert!(plan(&world).await.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn total_open_executions_have_their_own_higher_cap() {
    assert!(auto_dispatch::OPEN_EXECUTIONS_CAP > BUILD_HEAVY_CAP);
    let world = World::new(pipeline_manager()).await;
    for _ in 0..auto_dispatch::OPEN_EXECUTIONS_CAP {
        seed_execution(&world, "land", "waiting").await;
    }
    labelled(&world, "ready", &[AUTO_LABEL], None).await;
    assert!(plan(&world).await.is_empty());
}
