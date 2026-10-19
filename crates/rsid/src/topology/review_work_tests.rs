//! #2607: a topology execution settles the review Work rows it owns: a row
//! whose accepted source the land node published records integration, and a
//! row of an execution that ended without landing it is retired.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    AgentTopologyExecuteRequestV1, AgentTopologyUpsertResultV1, AttemptStatus, KNOBS, ReviewScript,
    Step, TopologyDefinition, World, agent, luna, policy, review_definition, sonnet,
    upsert_request,
};
use crate::topology::land::LandStatus;
use crate::topology::recovery::settlement_cleanup;
use rsi_common::harness_manager_v2::ManagerCapabilityV2;

const LANDED: &str = "1111111111111111111111111111111111111111";
const UNLANDED: &str = "2222222222222222222222222222222222222222";
const TIP: &str = "3333333333333333333333333333333333333333";

/// A review row with a recorded acceptance of `source`, as the ledger holds it
/// after an admitted review.
fn review_row(key: &str, epic: Uuid, author: Uuid, source: &str) -> Value {
    json!({
        "key": key, "epic_id": epic, "title": "topology review",
        "kind": "program", "priority": 5, "weight": 1,
        "required_gates": ["implementation", "review"],
        "risk_tier": "tier1", "spec_revision": 1,
        "source_session_id": author, "source_commit": source,
        "stages": [],
        "acceptance": {"source_commit": source, "spec_revision": 1,
            "evidence_digest": "d", "method": "independent_evidence",
            "accepted_at": "2026-10-15T00:00:00Z"},
        "integration": null, "pending_acceptance": null,
    })
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn published_land_integrates_review_rows_and_a_settled_execution_retires_the_rest() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("review-work", review_definition(&sonnet(), &luna())),
        )
        .await
        .unwrap();
    let execution = world
        .execute(world.manager, &flow, world.epic_a, "review-work-run")
        .await
        .unwrap()
        .execution_id;
    let landed_key = format!("topology:{execution}:review");
    let unlanded_key = format!("topology:{execution}:post_review");
    let config = {
        let store = world.harness.executor.store.lock().await;
        let config = store.get_harness_manager(world.project).unwrap().unwrap();
        for (key, source) in [(&landed_key, LANDED), (&unlanded_key, UNLANDED)] {
            store
                .manager_v2_put_record(
                    &config,
                    "work",
                    key,
                    Some(world.epic_a),
                    0,
                    &review_row(key, world.epic_a, world.lead_a, source),
                )
                .unwrap();
        }
        config
    };
    let integration = |key: String| {
        let world = &world;
        let config = config.clone();
        async move {
            let store = world.harness.executor.store.lock().await;
            store
                .manager_v2_work(&config, &key)
                .unwrap()
                .1
                .integration
                .map(|integration| (integration.source_commit, integration.target_commit))
        }
    };

    // Nothing has landed yet: a pass while the execution runs changes nothing.
    let report = world
        .harness
        .executor
        .settle_review_work(execution, world.harness.repo.clone(), false)
        .await
        .unwrap();
    assert_eq!((report.integrated, report.retired, report.open), (0, 0, 2));

    // The land node published LANDED: its row records integration at the tip,
    // the other row stays open while the execution runs.
    world.harness.set_source_landed(LANDED, TIP);
    let report = world
        .harness
        .executor
        .settle_review_work(execution, world.harness.repo.clone(), false)
        .await
        .unwrap();
    assert_eq!((report.integrated, report.retired, report.open), (1, 0, 1));
    assert_eq!(
        integration(landed_key.clone()).await,
        Some((LANDED.to_owned(), TIP.to_owned()))
    );
    assert_eq!(integration(unlanded_key.clone()).await, None);

    // The execution ends without landing the second row: settlement retires it.
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topology_executions SET status='failed',finished_at=?2 WHERE id=?1",
                rusqlite::params![execution.to_string(), chrono::Utc::now().to_rfc3339()],
            )
            .unwrap();
    }
    settlement_cleanup(&world.harness.executor, 16).await;
    let store = world.harness.executor.store.lock().await;
    assert!(
        store
            .topology_open_review_work(execution)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .manager_v2_record(&config, "work", &unlanded_key)
            .unwrap()
            .unwrap()
            .archived
    );
    assert!(
        !store
            .manager_v2_record(&config, "work", &landed_key)
            .unwrap()
            .unwrap()
            .archived
    );
    let settled: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM topology_events WHERE execution_id=?1 AND kind='review_work_settled'",
            [execution.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(settled, 1, "settlement is recorded once");
    drop(store);
    // A second pass is a no-op.
    settlement_cleanup(&world.harness.executor, 16).await;
    let store = world.harness.executor.store.lock().await;
    let settled: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM topology_events WHERE execution_id=?1 AND kind='review_work_settled'",
            [execution.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(settled, 1);
}

/// Author `A` -> reviewer `R` -> land `L` (no Issue close).
fn land_definition() -> TopologyDefinition {
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
                "step": {"kind": "land", "accepted": "R", "test_filters": []},
            }},
        ],
        "edges": [{"from": "A", "to": "R"}, {"from": "R", "to": "L"}],
    }))
    .unwrap()
}

/// #2613: the land node's own published outcome integrates the execution's
/// review Work row. The row is read after executor ticks only: no direct
/// `settle_review_work` call and no settlement pass, so a missing or misplaced
/// call from the land observer leaves the row open and fails here.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_published_land_status_integrates_the_review_work_row_at_the_published_tip() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let flow: AgentTopologyUpsertResultV1 = world
        .upsert(
            world.lead_a,
            &upsert_request("land-work", land_definition()),
        )
        .await
        .unwrap();
    let execution = agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        world.manager,
        &AgentTopologyExecuteRequestV1 {
            project_id: None,
            on_call: None,
            supersedes_execution: None,
            wake: None,
            topology_id: flow.topology_id.unwrap(),
            expected_digest: flow.definition_digest.clone(),
            epic_id: world.epic_a,
            inputs: json!({}),
            base_commit: Some(world.harness.base.clone()),
            idempotency_key: "land-work-run".into(),
        },
    )
    .await
    .unwrap()
    .result
    .execution_id;

    // Drive the execution until its land node waits on the merge queue.
    let harness = &world.harness;
    harness.script_reviews([ReviewScript::Accept]);
    let mut land = None;
    for _ in 0..8 {
        harness.executor.advance(execution).await.unwrap();
        for attempt in harness.attempts(execution).await {
            if attempt.status == AttemptStatus::Running {
                harness.commit_and_complete(attempt.session_id, "impl");
            }
        }
        land = harness
            .attempts(execution)
            .await
            .into_iter()
            .find(|attempt| attempt.node_id == "L" && attempt.status == AttemptStatus::Waiting);
        if land.is_some() {
            break;
        }
    }
    let land = land.expect("the land node never reached the queue");
    let (entry, source) = (land.land_entry_id.unwrap(), land.base_commit.clone());

    // The review node's Work row, as the ledger holds it after an admitted
    // review of the commit the land node carries.
    let key = format!("topology:{execution}:R");
    let config = {
        let store = harness.executor.store.lock().await;
        let config = store.get_harness_manager(world.project).unwrap().unwrap();
        store
            .manager_v2_put_record(
                &config,
                "work",
                &key,
                Some(world.epic_a),
                0,
                &review_row(&key, world.epic_a, world.lead_a, &source),
            )
            .unwrap();
        config
    };
    let integration = || async {
        let store = harness.executor.store.lock().await;
        store
            .manager_v2_work(&config, &key)
            .unwrap()
            .1
            .integration
            .map(|integration| (integration.source_commit, integration.target_commit))
    };

    // The entry is still queued: a tick leaves the row open.
    harness.set_land_state(entry, LandStatus::Pending);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    assert_eq!(integration().await, None);

    // The queue publishes the entry and the source is an ancestor of the tip:
    // one tick of the executor settles the land node and integrates the row.
    harness.set_source_landed(&source, TIP);
    harness.set_land_state(
        entry,
        LandStatus::Published {
            landed_sha: Some(TIP.to_owned()),
        },
    );
    harness.executor.advance(execution).await.unwrap();
    assert_eq!(
        harness.attempt(execution, "L", 0, 1).await.status,
        AttemptStatus::Succeeded
    );
    assert_eq!(integration().await, Some((source, TIP.to_owned())));
}

/// Historical executions settle their owned work before the overview is read;
/// the intent snapshot can still contain all the old versions without bloating
/// the wire row. An execution still running remains discoverable.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn two_hundred_settled_executions_have_a_bounded_overview_and_one_live_item() {
    use rsi_common::harness_manager_v2::{AgentManagerInspectRequestV2, ManagerInspectSectionV2};
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("bounded-work", review_definition(&sonnet(), &luna())),
        )
        .await
        .unwrap();
    let mut settled = Vec::new();
    let mut all_keys = Vec::new();
    let mut live_key = String::new();
    for n in 0..203 {
        let execution = world
            .execute(world.manager, &flow, world.epic_a, &format!("bounded-{n}"))
            .await
            .unwrap()
            .execution_id;
        let key = format!("topology:{execution}:post_review");
        let store = world.harness.executor.store.lock().await;
        let config = store.get_harness_manager(world.project).unwrap().unwrap();
        store
            .manager_v2_put_record(
                &config,
                "work",
                &key,
                Some(world.epic_a),
                0,
                &review_row(&key, world.epic_a, world.lead_a, LANDED),
            )
            .unwrap();
        if n < 202 {
            let status = match n {
                200 => "failed",
                201 => "cancelled",
                _ => "succeeded",
            };
            store
                .conn
                .execute(
                    "UPDATE topology_executions SET status=?2,finished_at=?3 WHERE id=?1",
                    rusqlite::params![
                        execution.to_string(),
                        status,
                        chrono::Utc::now().to_rfc3339()
                    ],
                )
                .unwrap();
            settled.push(execution);
        } else {
            live_key = key.clone();
        }
        all_keys.push(json!({"key":key,"version":1,"source_accepted":false,"integrated":false}));
    }
    // Only the successful executions' accepted source reached rolling.
    // The failed/cancelled rows model an unlanded source and retire instead.
    {
        let store = world.harness.executor.store.lock().await;
        store.conn.execute(
            "UPDATE harness_manager_v2_work_facts SET payload_json=json_set(payload_json,'$.source_commit',?1)
             WHERE kind='work' AND record_key IN (?2,?3)",
            rusqlite::params![UNLANDED, format!("topology:{}:post_review", settled[200]),
                format!("topology:{}:post_review", settled[201])],
        ).unwrap();
        let config = store.get_harness_manager(world.project).unwrap().unwrap();
        store
            .manager_v2_put_record(
                &config,
                "intent",
                &world.epic_a.to_string(),
                Some(world.epic_a),
                0,
                &json!({"epic_id":world.epic_a,"work_versions":all_keys}),
            )
            .unwrap();
    }
    world.harness.set_source_landed(LANDED, TIP);
    // Recovery's normal batch also backfills rows from before the fix.
    for _ in 0..13 {
        settlement_cleanup(&world.harness.executor, 16).await;
    }
    let store = world.harness.executor.store.lock().await;
    for execution in settled {
        assert_eq!(store.topology_open_review_work(execution).unwrap().len(), 0);
    }
    let overview = store
        .manager_v2_inspect(
            world.manager,
            &AgentManagerInspectRequestV2 {
                epic_id: Some(world.epic_a),
                ..Default::default()
            },
        )
        .unwrap();
    let intent = overview
        .rows
        .iter()
        .find(|row| row["type"] == "intent")
        .unwrap();
    assert_eq!(intent["work_versions_format"], "work_summary_v1");
    assert_eq!(intent["work_versions_count"], 203);
    assert_eq!(intent["work_counts"]["integrated"], 200);
    assert_eq!(intent["work_counts"]["retired"], 2);
    assert_eq!(intent["work_versions_unfinished_count"], 1);
    assert_eq!(intent["work_versions"].as_array().unwrap().len(), 1);
    assert_eq!(intent["work_versions"][0]["key"], live_key);
    assert!(serde_json::to_vec(intent).unwrap().len() < 2048);
    let program = overview
        .rows
        .iter()
        .find(|row| row["key"] == "program")
        .unwrap();
    assert_eq!(program["integrated"], 200);
    assert_eq!(program["partial"], 0);
    let mut query: AgentManagerInspectRequestV2 =
        serde_json::from_value(intent["work_query"].clone()).unwrap();
    assert_eq!(query.section, ManagerInspectSectionV2::Work);
    let mut rows = Vec::new();
    loop {
        let page = store.manager_v2_inspect(world.manager, &query).unwrap();
        rows.extend(page.rows);
        query.cursor = page.next_cursor;
        if query.cursor.is_none() {
            break;
        }
        assert!(rows.len() < 204);
    }
    assert_eq!(rows.len(), 203);
    let live = rows.iter().find(|row| row["key"] == live_key).unwrap();
    assert_eq!(live["integrated"], false);
    assert_eq!(live["retired"], false);
}
