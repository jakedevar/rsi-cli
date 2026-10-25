//! #2260 acceptance tests: a published land with `close_issue` closes the
//! execution's input Issue as its owner. A child module of `agent_tests`, so
//! it shares its `World` (a real project manager and Epic lead, with the
//! Issue authority the daemon derives from rows) and the provider-free
//! executor harness.

use super::*;
use crate::topology::issue_close::{IssueClose, IssueCloseRequest, close_input_issue};
use crate::topology::land::LandStatus;
use crate::topology::starters;
use rsi_common::rpc::AgentUpdateIssueStatusRequestV1;
use rsi_common::types::{Issue, IssueStatus, NewIssue};
use serde_json::{Value, json};

/// A project manager that may run topologies and coordinate Issues.
fn coordinator() -> ManagerPolicyV2 {
    policy(
        &[
            ManagerCapabilityV2::Automation,
            ManagerCapabilityV2::IssueCoordinate,
        ],
        vec![sonnet(), luna()],
        64,
    )
}

/// Author `A` -> reviewer `R` -> land `L`, closing the input Issue when asked.
fn closing_flow(close_issue: bool) -> TopologyDefinition {
    serde_json::from_value(json!({
        "nodes": [
            {"id": "A", "kind": "Task", "label": "A", "params": {
                "instructions": "do A",
                "provider": "claude",
                "model": sonnet().model,
                "effort": sonnet().effort,
                "step": {"kind": "session", "expects_commit": true},
            }},
            {"id": "R", "kind": "Task", "label": "R", "prereqs": ["A"], "params": {
                "step": {"kind": "review", "of": "A", "reviewer": {
                    "provider": "codex",
                    "model": luna().model,
                    "effort": luna().effort,
                }},
            }},
            {"id": "L", "kind": "Task", "label": "L", "prereqs": ["R"], "params": {
                "when": {"R": "verdict_accepted"},
                "step": {
                    "kind": "land",
                    "accepted": "R",
                    "test_filters": [],
                    "close_issue": close_issue,
                },
            }},
        ],
        "edges": [{"from": "A", "to": "R"}, {"from": "R", "to": "L"}],
    }))
    .unwrap()
}

pub(super) async fn open_issue(world: &World, title: &str) -> Issue {
    open_labeled_issue(world, title, &[]).await
}

/// [`open_issue`] carrying `labels` (#3772).
pub(super) async fn open_labeled_issue(world: &World, title: &str, labels: &[&str]) -> Issue {
    open_issue_with(world, title, "Fix it.", labels).await
}

/// [`open_labeled_issue`] with an explicit `body` (#3808).
async fn open_issue_with(world: &World, title: &str, body: &str, labels: &[&str]) -> Issue {
    let store = world.harness.executor.store.lock().await;
    store
        .create_issue(&NewIssue {
            project_id: world.project,
            title: title.into(),
            body: body.into(),
            priority: None,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        })
        .unwrap()
}

pub(super) async fn issue_now(world: &World, issue: &Issue) -> Issue {
    let store = world.harness.executor.store.lock().await;
    store.get_issue(issue.id).unwrap().unwrap()
}

/// Idempotency keys of the Issue's status events, oldest first.
pub(super) async fn status_events(world: &World, issue: &Issue) -> Vec<String> {
    let store = world.harness.executor.store.lock().await;
    let mut statement = store
        .conn
        .prepare(
            "SELECT idempotency_key FROM issue_events \
             WHERE issue_id=?1 AND operation='status_updated' ORDER BY sequence",
        )
        .unwrap();
    statement
        .query_map([issue.id.to_string()], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|key| key.unwrap())
        .collect()
}

/// Payloads of the execution's topology events of one kind.
pub(super) async fn topology_events(world: &World, execution: Uuid, kind: &str) -> Vec<Value> {
    let store = world.harness.executor.store.lock().await;
    let mut statement = store
        .conn
        .prepare(
            "SELECT payload_json FROM topology_events \
             WHERE execution_id=?1 AND kind=?2 ORDER BY execution_seq",
        )
        .unwrap();
    statement
        .query_map(rusqlite::params![execution.to_string(), kind], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .map(|text| serde_json::from_str(&text.unwrap()).unwrap())
        .collect()
}

async fn upsert_flow(world: &World, name: &str, close_issue: bool) -> AgentTopologyUpsertResultV1 {
    world
        .upsert(
            world.lead_a,
            &upsert_request(name, closing_flow(close_issue)),
        )
        .await
        .unwrap()
}

/// Execute `flow` for the project manager with `inputs`.
async fn start(
    world: &World,
    flow: &AgentTopologyUpsertResultV1,
    inputs: Value,
    key: &str,
) -> Result<Uuid> {
    let request = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        supersedes_execution: None,
        wake: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest.clone(),
        epic_id: world.epic_a,
        inputs,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: key.into(),
    };
    agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        world.manager,
        &request,
    )
    .await
    .map(|executed| executed.result.execution_id)
}

/// `start` that also names the execution to supersede (#2427).
async fn start_superseding(
    world: &World,
    flow: &AgentTopologyUpsertResultV1,
    inputs: Value,
    key: &str,
    supersedes: Option<Uuid>,
) -> Result<Uuid> {
    let request = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        supersedes_execution: supersedes,
        wake: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest.clone(),
        epic_id: world.epic_a,
        inputs,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: key.into(),
    };
    agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        world.manager,
        &request,
    )
    .await
    .map(|executed| executed.result.execution_id)
}

/// Interrupt `execution` as the manager and drive it to `Cancelled`.
async fn cancel(world: &World, execution: Uuid, key: &str) {
    if world.harness.status(execution).await != ExecutionStatus::Cancelling {
        interrupt_now(world, execution, key).await;
    }
    for attempt in world.harness.attempts(execution).await {
        world
            .harness
            .set_status(attempt.session_id, SessionStatus::Interrupted);
    }
    for _ in 0..6 {
        if world.harness.executor.advance(execution).await.unwrap() == Step::Done {
            break;
        }
    }
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Cancelled
    );
}

async fn interrupt_now(world: &World, execution: Uuid, key: &str) {
    world.harness.executor.advance(execution).await.unwrap();
    let version: i64 = {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .query_row(
                "SELECT row_version FROM topology_executions WHERE id=?1",
                [execution.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    };
    world
        .interrupt(world.manager, execution, version, key)
        .await
        .unwrap();
}

/// Run an execution of `closing_flow(true)` for `issue` until its land node
/// waits on the merge queue.
async fn land_waiting(world: &World, issue: &Issue, key: &str) -> (Uuid, rows::AttemptRow) {
    let flow = upsert_flow(world, key, true).await;
    let execution = start(world, &flow, json!({"issue": issue.display_number}), key)
        .await
        .unwrap();
    let harness = &world.harness;
    harness.script_reviews([ReviewScript::Accept]);
    for _ in 0..8 {
        harness.executor.advance(execution).await.unwrap();
        for attempt in harness.attempts(execution).await {
            if attempt.status == AttemptStatus::Running {
                harness.commit_and_complete(attempt.session_id, "impl");
            }
        }
        if let Some(land) = harness
            .attempts(execution)
            .await
            .into_iter()
            .find(|attempt| attempt.node_id == "L")
            && land.status == AttemptStatus::Waiting
        {
            return (execution, land);
        }
    }
    panic!("the land node never reached the queue");
}

fn published(world: &World, land: &rows::AttemptRow) {
    world.harness.set_land_state(
        land.land_entry_id.unwrap(),
        LandStatus::Published {
            landed_sha: Some("f".repeat(40)),
        },
    );
}

async fn settled_land(world: &World, execution: Uuid) -> rows::AttemptRow {
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    world.harness.attempt(execution, "L", 0, 1).await
}

/// The land's `issue_close` output field as `(status, code)`.
fn issue_close(land: &rows::AttemptRow) -> (String, Option<String>) {
    let field = &land.output.as_ref().unwrap()["fields"]["issue_close"];
    (
        field["status"].as_str().unwrap().to_owned(),
        field["code"].as_str().map(str::to_owned),
    )
}

async fn close_by_hand(world: &World, issue: &Issue, status: IssueStatus, key: &str) {
    let store = world.harness.executor.store.lock().await;
    store
        .agent_update_issue_status(
            world.manager,
            &AgentUpdateIssueStatusRequestV1 {
                note: None,
                issue_id: Some(issue.id),
                display_number: None,
                status,
                expected_row_version: issue.row_version,
                idempotency_key: key.into(),
                include_issue: false,
                project_id: None,
            },
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_already_on_rolling_succeeds_and_closes_its_issue() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Already contained source").await;
    let (execution, land) = land_waiting(&world, &issue, "already-contained").await;
    let tip = "c".repeat(40);
    let result = crate::rolling_queue::classify_run(
        Some(0),
        &format!(
            "publication_status=published\nlanding_outcome=already_on_rolling\npublished_target_id={tip}\n"
        ),
        "",
        || panic!("typed success already carries its containing tip"),
    );
    assert_eq!(result.outcome.detail.as_deref(), Some("already_on_rolling"));
    world.harness.set_land_state(
        land.land_entry_id.unwrap(),
        LandStatus::from_queue(result.state, Some(result.outcome)),
    );
    let land = settled_land(&world, execution).await;
    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(land.output.as_ref().unwrap()["fields"]["landed_sha"], tip);
    assert_eq!(issue_close(&land), ("closed".to_owned(), None));
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_published_closes_the_input_issue_with_a_landed_note() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Close me").await;
    assert_eq!(issue.status, IssueStatus::Open);
    let (execution, land) = land_waiting(&world, &issue, "close-me").await;
    let entry = land.land_entry_id.unwrap();

    // Nothing is closed while the entry is still queued; the run holds it.
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
    published(&world, &land);
    let land = settled_land(&world, execution).await;

    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(issue_close(&land), ("closed".to_owned(), None));
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    let closed = issue_now(&world, &issue).await;
    assert_eq!(closed.status, IssueStatus::Closed);
    assert!(closed.closed_at.is_some());

    // The close is the owner's own guarded status write, keyed by the
    // execution and the land attempt.
    assert_eq!(
        status_events(&world, &issue).await,
        vec![
            format!("topology-start:{execution}"),
            format!("topology-land:{execution}:{}", land.id)
        ]
    );
    // One note names the execution, node, landed SHA, source commit, entry.
    let notes = topology_events(&world, execution, "topology_landed").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    let detail = &notes[0]["detail"];
    assert_eq!(detail["execution_id"], execution.to_string());
    assert_eq!(detail["node_id"], "L");
    assert_eq!(detail["landed_sha"], "f".repeat(40));
    assert_eq!(detail["source_commit"], land.base_commit);
    assert_eq!(detail["entry_id"], entry.to_string());
    assert_eq!(detail["issue"], issue.display_number);
    assert!(
        topology_events(&world, execution, "issue_close_refused")
            .await
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_close_issue_replay_after_crash_closes_once() {
    // A crash between the Issue write and the land's settlement: the next
    // tick re-runs the close, which recognises its own write, and the land
    // settles once with one note.
    let mut world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Crash between close and settle").await;
    let (execution, land) = land_waiting(&world, &issue, "crash-close").await;
    {
        let store = world.harness.executor.store.lock().await;
        let row = rows::load_execution(&store, execution).unwrap().unwrap();
        let request = IssueCloseRequest::for_land(&row, &land).unwrap().unwrap();
        assert_eq!(
            close_input_issue(&store, &request).unwrap(),
            IssueClose::Closed
        );
        // The same call again is the replay: no second write.
        assert_eq!(
            close_input_issue(&store, &request).unwrap(),
            IssueClose::Closed
        );
    }
    let closed = issue_now(&world, &issue).await;
    assert_eq!(closed.status, IssueStatus::Closed);
    world.harness.restart();
    published(&world, &land);
    let land = settled_land(&world, execution).await;

    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(issue_close(&land), ("closed".to_owned(), None));
    assert_eq!(status_events(&world, &issue).await.len(), 2);
    assert_eq!(
        issue_now(&world, &issue).await.row_version,
        closed.row_version,
        "the replay wrote nothing"
    );
    assert_eq!(
        topology_events(&world, execution, "topology_landed")
            .await
            .len(),
        1
    );

    // A crash after the note but before the settlement leaves one note too.
    let issue = open_issue(&world, "Crash after the note").await;
    let (execution, land) = land_waiting(&world, &issue, "crash-note").await;
    {
        let store = world.harness.executor.store.lock().await;
        let row = rows::load_execution(&store, execution).unwrap().unwrap();
        let request = IssueCloseRequest::for_land(&row, &land).unwrap().unwrap();
        assert_eq!(
            close_input_issue(&store, &request).unwrap(),
            IssueClose::Closed
        );
        rows::note_land_issue(
            &store,
            execution,
            &land,
            Some(world.manager),
            "topology_landed",
            json!({"pre": "written"}),
        )
        .unwrap()
        .expect("the first note is written");
    }
    world.harness.restart();
    published(&world, &land);
    let land = settled_land(&world, execution).await;
    assert_eq!(issue_close(&land), ("closed".to_owned(), None));
    let notes = topology_events(&world, execution, "topology_landed").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["detail"]["pre"], "written");
    assert_eq!(status_events(&world, &issue).await.len(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_close_issue_already_closed_is_recorded_not_repeated() {
    let world = World::new(coordinator()).await;

    // Closed by someone else before the entry published: recorded, no write,
    // no second note.
    let issue = open_issue(&world, "Closed by hand").await;
    close_by_hand(&world, &issue, IssueStatus::Closed, "by-hand").await;
    let by_hand = issue_now(&world, &issue).await;
    let (execution, land) = land_waiting(&world, &issue, "already-closed").await;
    published(&world, &land);
    let land = settled_land(&world, execution).await;
    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(issue_close(&land), ("already_closed".to_owned(), None));
    assert_eq!(
        status_events(&world, &issue).await,
        vec!["by-hand".to_owned()]
    );
    assert_eq!(
        issue_now(&world, &issue).await.row_version,
        by_hand.row_version
    );
    assert!(
        topology_events(&world, execution, "topology_landed")
            .await
            .is_empty()
    );

    // A cancelled Issue is untouched.
    let cancelled = open_issue(&world, "Cancelled").await;
    close_by_hand(&world, &cancelled, IssueStatus::Cancelled, "cancel").await;
    let (execution, land) = land_waiting(&world, &cancelled, "skip-cancelled").await;
    published(&world, &land);
    let land = settled_land(&world, execution).await;
    assert_eq!(
        issue_close(&land),
        ("skipped".to_owned(), Some("issue_cancelled".to_owned()))
    );
    assert_eq!(
        issue_now(&world, &cancelled).await.status,
        IssueStatus::Cancelled
    );

    // So is an archived one.
    let archived = open_issue(&world, "Archived").await;
    let (execution, land) = land_waiting(&world, &archived, "skip-archived").await;
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE issues SET archived_at='2026-10-13T00:00:00.000000000Z' WHERE id=?1",
                [archived.id.to_string()],
            )
            .unwrap();
    }
    published(&world, &land);
    let land = settled_land(&world, execution).await;
    assert_eq!(
        issue_close(&land),
        ("skipped".to_owned(), Some("issue_archived".to_owned()))
    );
    assert_eq!(
        issue_now(&world, &archived).await.status,
        IssueStatus::InProgress
    );
    // Only the start mark: the skipped close wrote nothing.
    assert_eq!(
        status_events(&world, &archived).await,
        vec![format!("topology-start:{execution}")]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_close_issue_refused_authority_settles_published_with_refusal_output() {
    // The manager may run topologies but holds no Issue authority.
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let issue = open_issue(&world, "Owner cannot close").await;
    let (execution, land) = land_waiting(&world, &issue, "refused-authority").await;
    published(&world, &land);
    let land = settled_land(&world, execution).await;

    // The commit is on rolling, so the land stands; the refusal is data.
    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(
        land.output.as_ref().unwrap()["fields"]["state"],
        "published"
    );
    assert_eq!(
        issue_close(&land),
        (
            "refused".to_owned(),
            Some("agent_issue_authority_denied".to_owned())
        )
    );
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    // The Issue stays open for the owner, with one event saying why.
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Open);
    assert!(status_events(&world, &issue).await.is_empty());
    let refused = topology_events(&world, execution, "issue_close_refused").await;
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["detail"]["code"], "agent_issue_authority_denied");
    assert_eq!(refused[0]["detail"]["node_id"], "L");
    assert!(
        topology_events(&world, execution, "topology_landed")
            .await
            .is_empty()
    );

    // The authority is re-derived at the close, not remembered from accept:
    // an owner whose grant is revoked while the entry gates is refused too.
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Grant revoked while gating").await;
    let (execution, land) = land_waiting(&world, &issue, "revoked").await;
    world
        .grant(policy(
            &[ManagerCapabilityV2::Automation],
            vec![sonnet(), luna()],
            64,
        ))
        .await;
    published(&world, &land);
    let land = settled_land(&world, execution).await;
    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(issue_close(&land).0, "refused");
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
}

/// #3273/#3274: the project manager seat is re-appointed (the operator seats a
/// fresh session) while the land waits. The close is authorized as the current
/// appointed manager when its scope covers the execution's Epic.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_close_issue_after_a_reappointment_closes_as_the_current_manager() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Displaced owner").await;
    let (execution, land) = land_waiting(&world, &issue, "displaced-owner").await;
    let seat =
        super::reappointment_tests::reappoint(&world, vec![world.epic_a, world.epic_b]).await;
    world.grant(coordinator()).await;
    published(&world, &land);
    let land = settled_land(&world, execution).await;

    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(issue_close(&land), ("closed".to_owned(), None));
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    let actor: String = {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .query_row(
                "SELECT actor_session_id FROM issue_events WHERE issue_id=?1 AND idempotency_key=?2",
                rusqlite::params![
                    issue.id.to_string(),
                    format!("topology-land:{execution}:{}", land.id)
                ],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(actor, seat.to_string(), "the current seat wrote the close");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_close_issue_after_a_reappointment_that_drops_the_epic_is_refused() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Epic left the scope").await;
    let (execution, land) = land_waiting(&world, &issue, "epic-dropped").await;
    super::reappointment_tests::reappoint(&world, vec![world.epic_b]).await;
    world.grant(coordinator()).await;
    published(&world, &land);
    let land = settled_land(&world, execution).await;

    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(
        issue_close(&land),
        ("refused".to_owned(), Some("epic_out_of_scope".to_owned()))
    );
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_refused_entry_leaves_the_issue_open() {
    for state in ["refused", "failed", "superseded"] {
        let world = World::new(coordinator()).await;
        let issue = open_issue(&world, "Never closed by a refused entry").await;
        let (execution, land) = land_waiting(&world, &issue, state).await;
        world.harness.set_land_state(
            land.land_entry_id.unwrap(),
            LandStatus::Refused {
                state: state.into(),
                reason: "queue_batch_merge_conflict".into(),
            },
        );
        let land = settled_land(&world, execution).await;
        assert_eq!(land.status, AttemptStatus::Failed, "{state}");
        assert!(
            land.output.as_ref().unwrap()["fields"]
                .get("issue_close")
                .is_none(),
            "{state}"
        );
        let after = issue_now(&world, &issue).await;
        // The failed run returns the Issue it held (#2427): never closed.
        assert_eq!(after.status, IssueStatus::Open, "{state}");
        assert_eq!(
            status_events(&world, &issue).await,
            vec![
                format!("topology-start:{execution}"),
                format!("topology-release:{execution}")
            ],
            "{state}"
        );
        assert!(
            topology_events(&world, execution, "topology_landed")
                .await
                .is_empty()
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_without_close_issue_leaves_the_issue_and_the_output_alone() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Not closed by a plain land").await;
    let flow = upsert_flow(&world, "plain-land", false).await;
    let execution = start(
        &world,
        &flow,
        json!({"issue": issue.display_number}),
        "plain-land",
    )
    .await
    .unwrap();
    world.harness.script_reviews([ReviewScript::Accept]);
    let land = loop {
        world.harness.executor.advance(execution).await.unwrap();
        for attempt in world.harness.attempts(execution).await {
            if attempt.status == AttemptStatus::Running {
                world
                    .harness
                    .commit_and_complete(attempt.session_id, "impl");
            }
        }
        if let Some(land) = world
            .harness
            .attempts(execution)
            .await
            .into_iter()
            .find(|attempt| attempt.node_id == "L" && attempt.status == AttemptStatus::Waiting)
        {
            break land;
        }
    };
    published(&world, &land);
    let land = settled_land(&world, execution).await;
    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert!(
        land.output.as_ref().unwrap()["fields"]
            .get("issue_close")
            .is_none()
    );
    // A plain land never closes it; the finished run leaves it as it marked it.
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn execute_refuses_close_issue_topology_without_issue_input() {
    let world = World::new(coordinator()).await;
    let flow = upsert_flow(&world, "closing", true).await;
    let issue = open_issue(&world, "Named by the input").await;

    // No input, an input that is not an Issue number: refused before any row.
    for (key, inputs) in [
        ("none", Value::Null),
        ("other-key", json!({"area": "x"})),
        ("not-a-number", json!({"issue": "#12"})),
    ] {
        let error = start(&world, &flow, inputs, key).await.unwrap_err();
        assert_eq!(error_code(&error), "issue_input_required", "{key}");
    }
    // A number naming no Issue keeps its own refusal.
    let error = start(&world, &flow, json!({"issue": 4242}), "unknown")
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), "issue_not_found");
    {
        let store = world.harness.executor.store.lock().await;
        let executions: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM topology_executions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(executions, 0, "a refused execute persists nothing");
    }

    // The Issue snapshot is accepted, and its identity is what the land closes.
    let execution = start(
        &world,
        &flow,
        json!({"issue": issue.display_number}),
        "with-issue",
    )
    .await
    .unwrap();
    let stored = {
        let store = world.harness.executor.store.lock().await;
        rows::load_execution(&store, execution)
            .unwrap()
            .unwrap()
            .input
    };
    assert_eq!(
        crate::topology::starters::issue_snapshot(stored.as_ref()),
        Some((world.project, issue.display_number))
    );

    // A topology that does not close needs no Issue.
    let plain = upsert_flow(&world, "plain", false).await;
    start(&world, &plain, Value::Null, "plain-run")
        .await
        .unwrap();

    // The operator path shares the check: it runs on the bridged snapshot.
    let bridged = crate::session::SessionManager::bridge_topology(&Topology {
        id: Uuid::new_v4(),
        name: "closing".into(),
        definition: closing_flow(true),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    })
    .unwrap();
    let steps = crate::topology::steps::WorkflowSteps::from_workflow(&bridged).unwrap();
    assert!(steps.has_close_issue() && steps.close_issue("L") && !steps.close_issue("R"));
    let unresolved = json!({"issue": issue.display_number});
    let snapshot = json!({"issue": {
        "display_number": issue.display_number,
        "title": "t",
        "body": "b",
        "project_id": world.project,
    }});
    for input in [None, Some(&Value::Null), Some(&unresolved)] {
        let error = crate::topology::starters::require_issue_input(&steps, input).unwrap_err();
        assert_eq!(error_code(&error), "issue_input_required");
    }
    crate::topology::starters::require_issue_input(&steps, Some(&snapshot)).unwrap();
}

/// #2260 added the optional `close_issue` land field. A definition that does
/// not use it keeps its canonical text, so a stored topology's digest never
/// moves just because the field was added. Check the current shared pins
/// (also used by #2249's `require_verified` test), and that
/// `close_issue: true` round-trips.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn starter_digest_unchanged_by_close_issue_default() {
    for &(name, expected) in super::starters_tests::EXISTING_STARTER_DIGESTS {
        let starter = starters::STARTERS
            .iter()
            .find(|starter| starter.name == name)
            .expect("starter");
        let text = starter.canonical_text().expect("text");
        assert_eq!(crate::topology::store::digest(&text), expected, "{name}");
        assert!(!text.contains("close_issue"), "{name}");
        // The snapshot an execution is accepted with re-serializes each step
        // through the typed enum: it must not grow the key either.
        for node in &starter.definition().expect("definition").nodes {
            if let Some(step) = node.step().expect("step") {
                let json = serde_json::to_string(&step).expect("step json");
                assert!(!json.contains("close_issue"), "{name}: {json}");
            }
        }
    }

    use rsi_common::types::TopologyStep;
    let plain: TopologyStep =
        serde_json::from_value(json!({"kind": "land", "accepted": "R"})).unwrap();
    assert!(matches!(
        &plain,
        TopologyStep::Land {
            close_issue: false,
            ..
        }
    ));
    assert_eq!(
        serde_json::to_value(&plain).unwrap(),
        json!({"kind": "land", "accepted": "R", "test_filters": []})
    );
    let closing: TopologyStep =
        serde_json::from_value(json!({"kind": "land", "accepted": "R", "close_issue": true}))
            .unwrap();
    assert_eq!(
        serde_json::to_value(&closing).unwrap(),
        json!({"kind": "land", "accepted": "R", "test_filters": [], "close_issue": true})
    );
    // `false` spelled out reads back as the default and canonicalises away.
    let spelled: TopologyStep = serde_json::from_value(
        json!({"kind": "land", "accepted": "R", "test_filters": [], "close_issue": false}),
    )
    .unwrap();
    assert_eq!(spelled, plain);
    // Unknown fields stay refused.
    assert!(
        serde_json::from_value::<TopologyStep>(
            json!({"kind": "land", "accepted": "R", "close_issues": true})
        )
        .is_err()
    );
}

// ─── #2427: one Issue, one live run ────────────────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn second_execute_on_a_live_issue_is_refused_naming_it_and_admitted_after_it_settles() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "One run at a time").await;
    let flow = upsert_flow(&world, "one-run", true).await;
    let inputs = || json!({"issue": issue.display_number});

    let first = start(&world, &flow, inputs(), "first").await.unwrap();
    // The live execution marks its Issue InProgress, noting the execution.
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
    assert_eq!(
        status_events(&world, &issue).await,
        vec![format!("topology-start:{first}")]
    );

    let refused = start(&world, &flow, inputs(), "second").await.unwrap_err();
    assert_eq!(error_code(&refused), "topology_issue_already_live");
    assert!(
        format!("{refused:?}").contains(&first.to_string()),
        "{refused:?}"
    );

    // It ends without landing: the Issue is Open again, and a new run is admitted.
    cancel(&world, first, "stop-first").await;
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Open);
    assert!(
        status_events(&world, &issue)
            .await
            .contains(&format!("topology-release:{first}"))
    );
    let second = start(&world, &flow, inputs(), "second").await.unwrap();
    assert_ne!(second, first);
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn supersedes_execution_interrupts_the_live_holder_and_must_name_it() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Replace the run").await;
    let other = open_issue(&world, "Another Issue").await;
    let flow = upsert_flow(&world, "replace-run", true).await;
    let inputs = json!({"issue": issue.display_number});

    let first = start(&world, &flow, inputs.clone(), "first").await.unwrap();
    // Naming an execution that does not hold this Issue is refused.
    let elsewhere = start(
        &world,
        &flow,
        json!({"issue": other.display_number}),
        "other",
    )
    .await
    .unwrap();
    let wrong = start_superseding(&world, &flow, inputs.clone(), "wrong", Some(elsewhere))
        .await
        .unwrap_err();
    assert_eq!(error_code(&wrong), "topology_supersedes_not_live");
    assert_eq!(world.harness.status(first).await, ExecutionStatus::Accepted);

    let second = start_superseding(&world, &flow, inputs, "second", Some(first))
        .await
        .unwrap();
    assert_eq!(
        world.harness.status(first).await,
        ExecutionStatus::Cancelling
    );
    assert_eq!(
        world.harness.status(second).await,
        ExecutionStatus::Accepted
    );
    // The replaced run's end does not hand the Issue back: the new run holds it.
    cancel(&world, first, "stop-first").await;
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn execute_is_refused_while_an_issue_worker_holds_the_issue() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Held by a worker").await;
    let flow = upsert_flow(&world, "worker-held", true).await;
    let action = Uuid::new_v4();
    let worker = Uuid::new_v4();
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_operations (id,project_id,manager_session_id,\
                 scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,\
                 state,target_session_id,not_before,created_at,updated_at) \
                 VALUES (?1,?2,?3,1,0,'worker-launch','f','lifecycle_action',?4,'running',?5,\
                 '2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z',\
                 '2026-01-01T00:00:00.000000000Z')",
                rusqlite::params![
                    action.to_string(),
                    world.project.to_string(),
                    world.manager.to_string(),
                    json!({"issue_binding": {"issue_id": issue.id}}).to_string(),
                    worker.to_string(),
                ],
            )
            .unwrap();
    }
    let inputs = json!({"issue": issue.display_number});
    let refused = start(&world, &flow, inputs.clone(), "blocked")
        .await
        .unwrap_err();
    assert_eq!(error_code(&refused), "topology_issue_already_live");
    assert!(
        format!("{refused:?}").contains(&worker.to_string()),
        "{refused:?}"
    );
    // supersedes_execution replaces executions only, never a worker.
    let still = start_superseding(&world, &flow, inputs.clone(), "still", Some(Uuid::new_v4()))
        .await
        .unwrap_err();
    assert_eq!(error_code(&still), "topology_supersedes_not_live");

    world
        .harness
        .executor
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations SET state='failed' WHERE id=?1",
            [action.to_string()],
        )
        .unwrap();
    start(&world, &flow, inputs, "admitted").await.unwrap();
}

/// #2651: run `closing_flow(true)` for `issue` until its implement node `A`
/// runs, then hand off `already_satisfied` with `evidence(base)` and no commit.
async fn satisfied_run(
    world: &World,
    issue: &Issue,
    key: &str,
    evidence: impl FnOnce(&std::path::Path, &str) -> String,
) -> (Uuid, rows::AttemptRow) {
    satisfied_run_body(world, issue, key, evidence, None).await
}

/// [`satisfied_run`] with an explicit handoff body after `evidence_commit` (#3772).
async fn satisfied_run_body(
    world: &World,
    issue: &Issue,
    key: &str,
    evidence: impl FnOnce(&std::path::Path, &str) -> String,
    body: Option<&str>,
) -> (Uuid, rows::AttemptRow) {
    let flow = upsert_flow(world, key, true).await;
    let execution = start(world, &flow, json!({"issue": issue.display_number}), key)
        .await
        .unwrap();
    let harness = &world.harness;
    harness.executor.advance(execution).await.unwrap();
    let running = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(running.status, AttemptStatus::Running);
    let evidence = evidence(&harness.sandbox(running.session_id), &harness.base);
    match body {
        Some(body) => harness.complete_already_satisfied_with(running.session_id, &evidence, body),
        None => harness.complete_already_satisfied(running.session_id, &evidence),
    }
    harness.executor.advance(execution).await.unwrap();
    (execution, harness.attempt(execution, "A", 0, 1).await)
}

/// #2651: an `already_satisfied` handoff naming an ancestor of the base settles
/// the run `succeeded` without landing and closes the Issue with a note naming
/// the evidence commit and the worker's verification lines.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_handoff_closes_the_issue_without_landing() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Stale red").await;
    let (execution, author) =
        satisfied_run(&world, &issue, "stale-red", |_, base| base.into()).await;

    assert_eq!(author.status, AttemptStatus::Succeeded, "{author:?}");
    assert_eq!(author.result_commit, None);
    let fields = &author.output.as_ref().unwrap()["fields"];
    assert_eq!(fields["already_satisfied"], true);
    assert_eq!(fields["evidence_commit"], world.harness.base);
    assert_eq!(fields["handoff"]["status"], "already_satisfied");
    assert_eq!(fields["issue_close"]["status"], "closed");

    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    // Review and land are dead paths: skipped, no queue entry.
    for node in ["R", "L"] {
        let skipped = world.harness.attempt(execution, node, 0, 1).await;
        assert_eq!(skipped.status, AttemptStatus::Skipped, "{node}");
        assert_eq!(skipped.land_entry_id, None);
    }
    let closed = issue_now(&world, &issue).await;
    assert_eq!(closed.status, IssueStatus::Closed);
    assert_eq!(
        status_events(&world, &issue).await,
        vec![
            format!("topology-start:{execution}"),
            format!("topology-satisfied:{execution}:{}", author.id)
        ]
    );
    let note: String = world
        .harness
        .executor
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT request_json FROM issue_events \
             WHERE issue_id=?1 AND idempotency_key=?2",
            rusqlite::params![
                issue.id.to_string(),
                format!("topology-satisfied:{execution}:{}", author.id)
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert!(note.contains(&world.harness.base), "{note}");
    assert!(note.contains("cargo test -p x: ok 3/3"), "{note}");
    assert!(
        topology_events(&world, execution, "topology_landed")
            .await
            .is_empty()
    );
}

/// #3045: evidence reachable only from a supplied `base_commit` that is not on
/// `origin/rolling` is unlanded work, not already satisfied: the Issue stays
/// open, the node settles on the base as a `complete` handoff and the land node
/// asks the merge queue to land exactly that base.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_with_evidence_only_in_an_unlanded_base_runs_the_land_node() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Satellite fix").await;
    let base = world.harness.base.clone();
    world.harness.set_off_rolling(&base);
    world.harness.script_reviews([ReviewScript::Accept]);
    let (execution, author) =
        satisfied_run(&world, &issue, "satellite-fix", |_, base| base.into()).await;

    assert_eq!(author.status, AttemptStatus::Succeeded, "{author:?}");
    assert_eq!(author.result_commit.as_deref(), Some(base.as_str()));
    let fields = &author.output.as_ref().unwrap()["fields"];
    assert!(fields.get("already_satisfied").is_none(), "{fields}");
    assert_eq!(fields["result_commit"], base);
    assert_ne!(issue_now(&world, &issue).await.status, IssueStatus::Closed);

    for _ in 0..8 {
        world.harness.executor.advance(execution).await.unwrap();
        if !world.harness.land_requests().is_empty() {
            break;
        }
    }
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].1.source_commit, base);
    assert_ne!(
        world.harness.attempt(execution, "L", 0, 1).await.status,
        AttemptStatus::Skipped
    );
}

/// #3045: unlanded evidence is also refused `handoff_invalid` where the
/// supplied-base route does not apply (the base is not the execution's own
/// supplied base), so a retry never closes the Issue on work not on rolling.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_with_unlanded_evidence_is_refused_when_the_head_moved() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Moved head").await;
    let base = world.harness.base.clone();
    world.harness.set_off_rolling(&base);
    // A new commit alongside an already_satisfied claim: not the unchanged
    // supplied base, so the supplied-base route does not apply.
    let (_, author) = satisfied_run(&world, &issue, "moved-head", |sandbox, base| {
        std::fs::write(sandbox.join("extra.txt"), "extra").unwrap();
        git(sandbox, &["add", "-A"]);
        git(sandbox, &["commit", "-q", "-m", "extra"]);
        base.into()
    })
    .await;
    assert_eq!(author.status, AttemptStatus::Failed, "{author:?}");
    assert_eq!(
        author.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_INVALID)
    );
    assert!(
        author
            .error
            .as_deref()
            .unwrap()
            .contains("not on origin/rolling"),
        "{author:?}"
    );
    assert_ne!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
}

/// #2651: an evidence commit that is not an ancestor of the base (a commit the
/// repository has but the base lacks, or one it does not have) is still
/// `handoff_invalid`; the Issue stays open.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_with_a_non_ancestor_evidence_commit_is_handoff_invalid() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Not fixed").await;
    for (key, unrelated) in [("real-commit", true), ("missing-commit", false)] {
        let (_, author) = satisfied_run(&world, &issue, key, |sandbox, base| {
            if unrelated {
                // A commit that exists in the repository but is not in the
                // base's history: made, then the branch is put back.
                std::fs::write(sandbox.join("later.txt"), "later").unwrap();
                git(sandbox, &["add", "-A"]);
                git(sandbox, &["commit", "-q", "-m", "later"]);
                let later = git(sandbox, &["rev-parse", "HEAD"]);
                git(sandbox, &["reset", "-q", "--hard", base]);
                later
            } else {
                "1".repeat(40)
            }
        })
        .await;
        assert_eq!(author.status, AttemptStatus::Failed, "{key}: {author:?}");
        assert_eq!(
            author.failure_class.as_deref(),
            Some(rows::failure::HANDOFF_INVALID),
            "{key}"
        );
        assert!(
            author.error.as_deref().unwrap().contains("not an ancestor"),
            "{key}: {author:?}"
        );
        assert_ne!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    }
}

/// #2651: `already_satisfied` without an evidence commit does not parse; the
/// session is asked once to repeat its handoff, then the node fails.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn already_satisfied_without_evidence_commit_does_not_parse() {
    let reply = "PIPELINE HANDOFF — IMPLEMENTATION:\ndoc_path: inline\n\
                 manifest_path: inline\nstatus: already_satisfied\n";
    let error = rsi_common::agent_contract::parse_pipeline_handoff_v2(reply, "").unwrap_err();
    assert!(error.to_string().contains("evidence_commit"), "{error}");
}

/// #3772: an `already_satisfied` claim with no `summary` or no `tests` is
/// refused (`already_satisfied_unproven`); the node fails and the Issue stays
/// open, even though the evidence commit is a valid ancestor.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_without_proof_is_refused_and_the_issue_stays_open() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Bare claim").await;
    for (key, body) in [
        ("bare", ""),
        ("no-tests", "summary: all criteria hold\n"),
        ("no-summary", "tests: cargo test -p x ok\n"),
        ("empty-values", "summary: none\ntests: n/a\n"),
        // #3808: the right fields with the wrong shape.
        (
            "generic",
            "summary: all criteria hold\ntests: cargo test passed\n",
        ),
        ("bare-words", "summary: ok\ntests: cargo test ok\n"),
        (
            "no-result",
            "summary:\n- AC1: lib.rs:1\ntests:\n- cargo test passed\n",
        ),
    ] {
        let (_, author) =
            satisfied_run_body(&world, &issue, key, |_, base| base.into(), Some(body)).await;
        assert_eq!(author.status, AttemptStatus::Failed, "{key}: {author:?}");
        assert_eq!(
            author.failure_class.as_deref(),
            Some(rows::failure::HANDOFF_INVALID),
            "{key}"
        );
        assert!(
            author
                .error
                .as_deref()
                .unwrap()
                .contains("already_satisfied_unproven"),
            "{key}: {author:?}"
        );
        assert_ne!(
            issue_now(&world, &issue).await.status,
            IssueStatus::Closed,
            "{key}"
        );
    }
}

/// #3772: a proven `already_satisfied` claim on an `operator-request` Issue
/// settles the node but leaves the Issue open for a manager to acknowledge; the
/// output says `issue_close: held`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_never_auto_closes_an_operator_request_issue() {
    let world = World::new(coordinator()).await;
    let issue = open_labeled_issue(&world, "Operator asked", &["operator-request"]).await;
    let (execution, author) =
        satisfied_run(&world, &issue, "operator-request", |_, base| base.into()).await;

    assert_eq!(author.status, AttemptStatus::Succeeded, "{author:?}");
    let close = &author.output.as_ref().unwrap()["fields"]["issue_close"];
    assert_eq!(close["status"], "held", "{close}");
    assert_eq!(close["code"], "manager_acknowledgement_required", "{close}");
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_ne!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    assert_eq!(
        status_events(&world, &issue).await,
        vec![format!("topology-start:{execution}")]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn satisfied_proof_gap_reads_inline_and_block_values() {
    use crate::topology::issue_close::satisfied_proof_gap;
    let head = "PIPELINE HANDOFF — IMPLEMENTATION:\nstatus: already_satisfied\n";
    assert!(satisfied_proof_gap(head, 1).is_some());
    // One criterion: an inline `AC1:` summary and an inline `<command>: <result>` test.
    let inline = format!("{head}summary: AC1: lib.rs:10 asserts it\ntests: cargo test -p x: ok\n");
    assert_eq!(satisfied_proof_gap(&inline, 1), None);
    let added = format!("{head}summary: AC1: lib.rs:10\ntests_added: none\n");
    assert!(satisfied_proof_gap(&added, 1).is_some());
    let block =
        format!("{head}summary:\n- AC1: test a\n- AC2: test b\ntests:\n  cargo test -p x: ok\n");
    assert_eq!(satisfied_proof_gap(&block, 2), None);
    let empty_block = format!("{head}summary:\ntests: cargo test: ok\n");
    assert!(satisfied_proof_gap(&empty_block, 1).is_some());
}

/// #3808: the shape check rejects (a) generic claims, (b) bare words, (c) a
/// summary with fewer criterion lines than the Issue has criteria, and a
/// criterion with no evidence; it accepts one `AC<n>: <evidence>` line per
/// criterion plus `tests` entries naming a command and a result.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn satisfied_proof_gap_checks_the_proof_shape() {
    use crate::topology::issue_close::satisfied_proof_gap;
    let head = "PIPELINE HANDOFF — IMPLEMENTATION:\nstatus: already_satisfied\n";
    let rejected = |criteria: usize, body: &str| {
        let gap = satisfied_proof_gap(&format!("{head}{body}"), criteria);
        assert!(gap.is_some(), "{body:?} must be refused");
    };
    // (a) a generic claim, (b) bare words.
    rejected(1, "summary: all criteria hold\ntests: cargo test passed\n");
    rejected(1, "summary: ok\ntests: cargo test ok\n");
    // (c) fewer criterion lines than criteria.
    rejected(
        3,
        "summary:\n- AC1: a.rs:1\n- AC2: b.rs:2\ntests:\n- cargo test -p x: ok\n",
    );
    // A criterion without evidence, a repeated number, a non-criterion line.
    rejected(
        2,
        "summary:\n- AC1: a.rs:1\n- AC2:\ntests:\n- cargo test: ok\n",
    );
    rejected(
        2,
        "summary:\n- AC1: a.rs:1\n- AC1: b.rs:2\ntests:\n- cargo test: ok\n",
    );
    rejected(
        2,
        "summary:\n- AC1: a.rs:1\n- AC2: ok\ntests:\n- cargo test: ok\n",
    );
    // Tests that name no command or no result.
    rejected(1, "summary:\n- AC1: a.rs:1\ntests:\n- cargo test passed\n");
    rejected(
        1,
        "summary:\n- AC1: a.rs:1\ntests:\n- cargo test: ok\n- ran them all\n",
    );
    rejected(1, "summary:\n- AC1: a.rs:1\ntests: cargo test:\n");

    let accepted = format!(
        "{head}summary:\n- AC1: crates/x/src/lib.rs:10 returns None\n- AC2: test x::y\n\
         tests:\n- cargo test -p x --lib -- --exact x::y: ok (1 passed)\n- scoped_test: exit 0\n"
    );
    assert_eq!(satisfied_proof_gap(&accepted, 2), None);
}

/// #3808: the Issue's criteria are the list items under its "acceptance
/// criteria" heading or label; prose criteria count as one.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn issue_acceptance_criteria_count_reads_the_criteria_list() {
    use crate::topology::issue_close::issue_acceptance_criteria_count;
    assert_eq!(issue_acceptance_criteria_count("Fix it."), 1);
    assert_eq!(
        issue_acceptance_criteria_count("Acceptance tests: it must work."),
        1
    );
    let listed = "Intro\n- not a criterion\n\n## Acceptance criteria\n- one\n- [ ] two\n3. three\n   continued\n\n## Notes\n- other\n";
    assert_eq!(issue_acceptance_criteria_count(listed), 3);
    let labeled =
        "**Acceptance criteria:**\n1. a\n2) b\n\nPlain closing paragraph.\n- later bullet\n";
    assert_eq!(issue_acceptance_criteria_count(labeled), 2);
}

/// #3808: an Issue with two acceptance criteria needs an `AC<n>: <evidence>`
/// line for each. A summary with one line fails the node
/// `already_satisfied_unproven` and leaves the Issue open; one line per
/// criterion with `<command>: <result>` tests settles it and closes the Issue.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_needs_a_summary_line_per_issue_criterion() {
    let world = World::new(coordinator()).await;
    let body = "Fix it.\n\n## Acceptance criteria\n- the first holds\n- the second holds\n";
    let issue = open_issue_with(&world, "Two criteria", body, &[]).await;

    let short = "summary:\n- AC1: lib.rs:10\ntests:\n- cargo test -p x: ok\n";
    let (_, author) =
        satisfied_run_body(&world, &issue, "short", |_, base| base.into(), Some(short)).await;
    assert_eq!(author.status, AttemptStatus::Failed, "{author:?}");
    assert_eq!(
        author.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_INVALID)
    );
    let error = author.error.as_deref().unwrap();
    assert!(error.contains("already_satisfied_unproven"), "{error}");
    assert!(error.contains("AC2"), "{error}");
    assert_ne!(issue_now(&world, &issue).await.status, IssueStatus::Closed);

    let full = "summary:\n- AC1: lib.rs:10\n- AC2: lib.rs:20\ntests:\n- cargo test -p x: ok\n";
    let (_, author) =
        satisfied_run_body(&world, &issue, "full", |_, base| base.into(), Some(full)).await;
    assert_eq!(author.status, AttemptStatus::Succeeded, "{author:?}");
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
}

/// #3880: a `tests` result that reports a failure, an error or a nonzero exit
/// is no proof; a passing result (also one that counts zero failures) is.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn satisfied_proof_gap_rejects_failing_test_results() {
    use crate::topology::issue_close::satisfied_proof_gap;
    let head =
        "PIPELINE HANDOFF — IMPLEMENTATION:\nstatus: already_satisfied\nsummary:\n- AC1: a.rs:1\n";
    let gap = |test: &str| satisfied_proof_gap(&format!("{head}tests:\n- {test}\n"), 1);
    for failing in [
        "cargo test -p app: failed (3 failures)",
        "cargo test -p app: fail",
        "cargo test -p app: error",
        "cargo test -p app: red",
        "scoped_test: exit 1",
        "scoped_test: exit code 101",
        "scoped_test: exited with status 2",
        "cargo test -p app: 3 failed, 12 passed",
        "cargo test -p app: nonzero exit",
        "cargo test -p app: not passed",
        "cargo test -p app: 42",
    ] {
        assert!(gap(failing).is_some(), "{failing:?} must be refused");
    }
    for passing in [
        "cargo test -p app: ok",
        "cargo test -p app: passed (0 failed)",
        "cargo test -p app: ok. 3 passed; 0 failed",
        "cargo test -p app: green, no errors",
        "scoped_test: exit 0",
        "scoped_test: exit code 0",
    ] {
        assert_eq!(gap(passing), None, "{passing:?} must be accepted");
    }
}

/// #3880: an `already_satisfied` handoff whose tests report a failure fails the
/// node `already_satisfied_unproven` and leaves the Issue open; a passing
/// result still closes it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_with_a_failing_test_result_leaves_the_issue_open() {
    let world = World::new(coordinator()).await;
    let issue = open_issue(&world, "Red tests").await;
    for (key, test) in [
        ("failed", "cargo test -p x: failed (3 failures)"),
        ("exit-1", "scoped_test: exit 1"),
    ] {
        let body = format!("summary:\n- AC1: lib.rs:10\ntests:\n- {test}\n");
        let (_, author) =
            satisfied_run_body(&world, &issue, key, |_, base| base.into(), Some(&body)).await;
        assert_eq!(author.status, AttemptStatus::Failed, "{key}: {author:?}");
        assert!(
            author
                .error
                .as_deref()
                .unwrap()
                .contains("already_satisfied_unproven"),
            "{key}: {author:?}"
        );
        assert_ne!(
            issue_now(&world, &issue).await.status,
            IssueStatus::Closed,
            "{key}"
        );
    }
    let passing = "summary:\n- AC1: lib.rs:10\ntests:\n- cargo test -p x: ok (3 passed)\n";
    let (_, author) = satisfied_run_body(
        &world,
        &issue,
        "passing",
        |_, base| base.into(),
        Some(passing),
    )
    .await;
    assert_eq!(author.status, AttemptStatus::Succeeded, "{author:?}");
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
}

/// #3881: a proof validated against one Issue version closes only that
/// version. An acceptance criterion added after validation and before the
/// close refuses the close, leaves the Issue open and records why.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn already_satisfied_close_refuses_an_issue_edited_after_the_proof() {
    use crate::topology::issue_close::ISSUE_CHANGED_AFTER_PROOF;
    let world = World::new(coordinator()).await;
    let body = "Fix it.\n\n## Acceptance criteria\n- the first holds\n";
    let issue = open_issue_with(&world, "Edited mid-proof", body, &[]).await;
    let flow = upsert_flow(&world, "edited", true).await;
    let execution = start(
        &world,
        &flow,
        json!({"issue": issue.display_number}),
        "edited",
    )
    .await
    .unwrap();
    world.harness.executor.advance(execution).await.unwrap();
    let attempt = world.harness.attempt(execution, "A", 0, 1).await;

    let store = world.harness.executor.store.lock().await;
    let row = rows::load_execution(&store, execution).unwrap().unwrap();
    // The proof covers one criterion: validated at this row version.
    let proved = store.get_issue(issue.id).unwrap().unwrap();
    assert_eq!(
        crate::topology::issue_close::issue_acceptance_criteria_count(&proved.body),
        1
    );
    let request = IssueCloseRequest::for_already_satisfied(
        &row,
        &attempt,
        "note".into(),
        Some(proved.row_version),
    )
    .unwrap();
    // An operator adds a second criterion before the close.
    store
        .update_issue(
            issue.id,
            &rsi_common::types::IssueUpdate {
                body: Some(format!("{body}- the second holds\n")),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        close_input_issue(&store, &request).unwrap(),
        IssueClose::Refused(ISSUE_CHANGED_AFTER_PROOF.into())
    );
    let now = store.get_issue(issue.id).unwrap().unwrap();
    assert_ne!(now.status, IssueStatus::Closed);

    // A request fenced on the current version closes.
    let fresh = IssueCloseRequest::for_already_satisfied(
        &row,
        &attempt,
        "note".into(),
        Some(now.row_version),
    )
    .unwrap();
    assert_eq!(
        close_input_issue(&store, &fresh).unwrap(),
        IssueClose::Closed
    );
}
