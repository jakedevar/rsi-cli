//! #2982 acceptance tests: the active reviewer of a topology review
//! assignment reads that assignment's execution with `AgentTopologyGetExecution`
//! and nothing else of the topology family. A child module of `agent_tests`, so
//! it shares its `World`.

use super::*;
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rusqlite::params;

/// A reviewer session under Epic A with an `active` review assignment whose
/// request names `attempt` (the executor's `topology_attempt_id`).
async fn assign_reviewer(world: &World, attempt: Uuid) -> (Uuid, Uuid) {
    let reviewer = Uuid::new_v4();
    let assignment = Uuid::new_v4();
    let store = world.harness.executor.store.lock().await;
    let mut row =
        crate::session::agent_verbs::tests::test_session(reviewer, world.harness.repo.clone());
    row.project_id = Some(world.project);
    row.session_kind = SessionKind::Task;
    row.parent_id = Some(world.epic_a);
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    store
        .conn
        .execute(
            "INSERT INTO manager_review_assignments (
                assignment_id,project_id,epic_id,manager_session_id,scope_version,
                work_key,spec_revision,author_session_id,source_sha,reviewer_session_id,
                reviewer_invocation_id,reviewer_custody_id,reviewer_custody_generation,
                action_operation_id,state,row_version,request_json,request_fingerprint,
                created_at,updated_at)
             VALUES (?1,?2,?3,?4,1,'reviewer-read',1,?4,?5,?6,?7,?8,1,?9,
                     'active',1,?10,?11,?12,?12)",
            params![
                assignment.to_string(),
                world.project.to_string(),
                world.epic_a.to_string(),
                world.manager.to_string(),
                "0".repeat(40),
                reviewer.to_string(),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                serde_json::json!({ "topology_attempt_id": attempt }).to_string(),
                format!("sha256:{}", "0".repeat(64)),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )
        .unwrap();
    store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    (reviewer, assignment)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_assigned_reviewer_reads_its_execution_and_only_that() {
    let world = World::new(automation()).await;
    let first = world
        .upsert(
            world.lead_a,
            &upsert_request("rv-a", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let first = world
        .execute(world.lead_a, &first, world.epic_a, "rv-a-run")
        .await
        .unwrap();
    let second = world
        .upsert(
            world.lead_b,
            &upsert_request("rv-b", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let second = world
        .execute(world.lead_b, &second, world.epic_b, "rv-b-run")
        .await
        .unwrap();
    assert_eq!(
        world
            .harness
            .executor
            .advance(first.execution_id)
            .await
            .unwrap(),
        Step::Wait
    );
    let attempt = world.harness.attempt(first.execution_id, "A", 0, 1).await;
    let (reviewer, assignment) = assign_reviewer(&world, attempt.id).await;

    // The assigned reviewer reads the execution its assignment serves.
    let read = world.get(reviewer, first.execution_id).await.unwrap();
    assert_eq!(read.execution.execution_id, first.execution_id);
    assert_eq!(read.nodes.len(), 1);
    assert_eq!(read.nodes[0].attempt_id, attempt.id);

    // Another execution, and every other topology verb, stay refused.
    assert_eq!(
        error_code(&world.get(reviewer, second.execution_id).await.unwrap_err()),
        "authority_denied"
    );
    let version = world.harness.row_version(first.execution_id).await;
    assert_eq!(
        error_code(
            &world
                .interrupt(reviewer, first.execution_id, version, "rv-stop")
                .await
                .unwrap_err()
        ),
        "authority_denied"
    );
    assert_eq!(
        error_code(
            &world
                .list(reviewer, &AgentTopologyListRequestV1::default())
                .await
                .unwrap_err()
        ),
        "authority_denied"
    );
    // An ordinary worker still reads nothing.
    assert_eq!(
        error_code(
            &world
                .get(world.worker, first.execution_id)
                .await
                .unwrap_err()
        ),
        "authority_denied"
    );

    // The read ends with the assignment: once it is no longer active the
    // same caller is refused.
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE manager_review_assignments \
                 SET state='cancelled',row_version=row_version+1,updated_at=?2,terminal_at=?2 \
                 WHERE assignment_id=?1",
                params![assignment.to_string(), "2999-01-01T00:00:00.000000000Z"],
            )
            .unwrap();
    }
    assert_eq!(
        error_code(&world.get(reviewer, first.execution_id).await.unwrap_err()),
        "authority_denied"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn the_catalog_lists_get_execution_to_a_topology_reviewer_only() {
    use crate::store::agent_authority::{VerbRights, permitted_verb};
    let reviewer = VerbRights {
        reviewer: true,
        topology_reviewer: true,
        ..Default::default()
    };
    assert!(permitted_verb(
        AgentControlVerbV1::TopologyGetExecution,
        &reviewer
    ));
    for verb in [
        AgentControlVerbV1::TopologyList,
        AgentControlVerbV1::TopologyGet,
        AgentControlVerbV1::TopologyExecute,
        AgentControlVerbV1::TopologyInterrupt,
        AgentControlVerbV1::TopologyResolveAttempt,
    ] {
        assert!(!permitted_verb(verb, &reviewer), "{verb:?}");
    }
    // A reviewer of a non-topology assignment is not served the verb.
    let plain = VerbRights {
        reviewer: true,
        ..Default::default()
    };
    assert!(!permitted_verb(
        AgentControlVerbV1::TopologyGetExecution,
        &plain
    ));
}

/// #3020: the assigned reviewer reads the author's verified `scoped_test`
/// receipt and the merge-queue gate receipt, each keyed to its exact commit;
/// a manager reading the same execution gets no receipt content.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_assigned_reviewer_reads_the_scoped_test_and_gate_receipts() {
    let world = World::new(automation()).await;
    let first = world
        .upsert(
            world.lead_a,
            &upsert_request("rv-receipts", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let first = world
        .execute(world.lead_a, &first, world.epic_a, "rv-receipts-run")
        .await
        .unwrap();
    assert_eq!(
        world
            .harness
            .executor
            .advance(first.execution_id)
            .await
            .unwrap(),
        Step::Wait
    );
    let attempt = world.harness.attempt(first.execution_id, "A", 0, 1).await;
    let (reviewer, _assignment) = assign_reviewer(&world, attempt.id).await;

    let commit = "c".repeat(40);
    let job = Uuid::new_v4();
    let entry = Uuid::new_v4();
    let batch = Uuid::new_v4();
    let land = Uuid::new_v4();
    {
        let store = world.harness.executor.store.lock().await;
        // The author node settled on a passing `scoped_test` for `commit`.
        store
            .conn
            .execute(
                "UPDATE topology_node_attempts SET status='succeeded',result_commit=?2,\
                 output_json=?3 WHERE id=?1",
                params![
                    attempt.id.to_string(),
                    commit,
                    serde_json::json!({"fields": {"verification": {
                        "job_id": job.to_string(),
                        "head": commit,
                        "exit_code": 0,
                        "plan": "precise",
                        "failing_tests": [],
                        "known_failures": [{"test": "rsid tests::baseline_red", "issue": 77}],
                        "new_failures": [],
                    }}})
                    .to_string(),
                ],
            )
            .unwrap();
        // The merge queue published the entry its land node enqueued.
        store
            .conn
            .execute(
                "INSERT INTO rolling_queue_batches(id,state,member_ids_json,member_count,\
                    created_at,row_version) VALUES (?1,'published','[]',1,?2,1)",
                params![batch.to_string(), "2026-10-18T00:00:00.000000000Z"],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO rolling_queue_entries(id, project_id, repo_path, source_commit, \
                    source_session_id, binding, hot_files_json, test_filters_json, state, \
                    idempotency_key, enqueued_at, finished_at, row_version, batch_id) \
                 VALUES (?1,?2,'/tmp/repo',?3,?4,'unbound','[]','[\"rsid topology::\"]',\
                    'published','k3020',?5,?5,1,?6)",
                params![
                    entry.to_string(),
                    world.project.to_string(),
                    commit,
                    world.worker.to_string(),
                    "2026-10-18T00:00:00.000000000Z",
                    batch.to_string(),
                ],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,\
                    attempt_no,node_kind,status,dedup_key,session_id,base_commit,input_json,\
                    integrate_action_id,created_at,updated_at) \
                 VALUES (?1,?2,'land',0,1,'land','succeeded',?3,?4,?5,'{}',?6,?7,?7)",
                params![
                    land.to_string(),
                    first.execution_id.to_string(),
                    format!("dedup-{land}"),
                    world.worker.to_string(),
                    commit,
                    entry.to_string(),
                    "2026-10-18T00:00:00.000000000Z",
                ],
            )
            .unwrap();
    }

    let read = world.get(reviewer, first.execution_id).await.unwrap();
    let receipts = read.receipts.expect("the reviewer reads the receipts");
    assert_eq!(receipts.scoped_test.len(), 1);
    let scoped = &receipts.scoped_test[0];
    assert_eq!(scoped.attempt_id, attempt.id);
    assert_eq!(scoped.node_id, "A");
    assert_eq!(scoped.result_commit.as_deref(), Some(commit.as_str()));
    assert_eq!(scoped.head.as_deref(), Some(commit.as_str()));
    assert_eq!(scoped.job_id, Some(job));
    assert_eq!(scoped.exit_code, Some(0));
    assert_eq!(scoped.plan.as_deref(), Some("precise"));
    assert!(scoped.failing_tests.is_empty() && scoped.new_failures.is_empty());
    assert_eq!(scoped.known_failures.len(), 1);
    assert_eq!(scoped.known_failures[0].issue, 77);
    assert_eq!(scoped.unverified_reason, None);
    assert_eq!(receipts.gate.len(), 1);
    let gate = &receipts.gate[0];
    assert_eq!(gate.attempt_id, land);
    assert_eq!(gate.entry_id, entry);
    assert_eq!(gate.commit, commit);
    assert_eq!(gate.state, "published");
    assert_eq!(gate.test_filters, vec!["rsid topology::".to_owned()]);
    assert!(
        gate.log_glob
            .as_deref()
            .is_some_and(|glob| glob.contains(&batch.to_string()))
    );

    // A caller that is not the assigned reviewer reads the execution without
    // any receipt content, and an ordinary worker still reads nothing.
    let managed = world.get(world.lead_a, first.execution_id).await.unwrap();
    assert!(managed.receipts.is_none());
    assert_eq!(
        error_code(
            &world
                .get(world.worker, first.execution_id)
                .await
                .unwrap_err()
        ),
        "authority_denied"
    );
}
