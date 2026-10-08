//! #1641 S5a acceptance tests: seeded starter topologies and the Issue
//! snapshot input. A child module of `agent_tests`, so it shares its `World`.

use super::*;
use crate::topology::starters::{
    self, ISSUE_IMPLEMENT_REVIEW_LAND, ROLLING_QA_SWEEP, STARTERS, SeedOutcome,
};
use rsi_common::types::NewIssue;

fn launch(provider: SessionProvider, model: &str, effort: &str) -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider,
        model: model.into(),
        effort: Some(effort.into()),
    }
}

/// The live operator grant (global manager grant v6, re-read 2026-10-07):
/// Claude Opus/Sonnet and Codex Sol/Luna/Astra triples; Haiku 5.5 is not in
/// it and no starter may use Haiku.
fn live_grant() -> Vec<ManagerLaunchChoiceV2> {
    let claude = SessionProvider::Claude;
    let codex = SessionProvider::Codex;
    vec![
        launch(claude, "claude-opus-5-5", "high"),
        launch(claude, "claude-opus-5-5", "xhigh"),
        launch(claude, "claude-sonnet-5-5", "medium"),
        launch(claude, "claude-sonnet-5-5", "high"),
        launch(claude, "claude-sonnet-5-5", "xhigh"),
        launch(codex, "gpt-6.1-sol", "high"),
        launch(codex, "gpt-6.1-sol", "xhigh"),
        launch(codex, "gpt-6-luna", "high"),
        launch(codex, "gpt-6-astra", "high"),
    ]
}

fn starter_topology(name: &str) -> Topology {
    let starter = STARTERS.iter().find(|s| s.name == name).unwrap();
    Topology {
        id: starter.id,
        name: starter.name.into(),
        definition: starter.definition().unwrap(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

async fn topology_row(world: &World, name: &str) -> (i64, String, i64, String) {
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .query_row(
            "SELECT revision,owner_kind,shared,definition_json FROM topologies WHERE name=?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn starters_seed_idempotently_and_validate() {
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    let store = world.harness.executor.store.lock().await;
    let seeded = starters::seed_all(&store);
    assert_eq!(
        seeded,
        vec![
            (ISSUE_IMPLEMENT_REVIEW_LAND, SeedOutcome::Inserted),
            (ROLLING_QA_SWEEP, SeedOutcome::Inserted),
        ]
    );
    // A second boot changes nothing.
    assert!(
        starters::seed_all(&store)
            .iter()
            .all(|(_, outcome)| *outcome == SeedOutcome::Unchanged)
    );
    for starter in &STARTERS {
        let (revision, owner_kind, shared, text): (i64, String, i64, String) = store
            .conn
            .query_row(
                "SELECT revision,owner_kind,shared,definition_json FROM topologies WHERE id=?1",
                [starter.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((revision, owner_kind.as_str(), shared), (1, "operator", 1));
        // Stored exactly as an agent upsert would store it.
        assert_eq!(text, starter.canonical_text().unwrap());
    }

    // An unedited older seed advances; the revision bumps once.
    store
        .conn
        .execute(
            "UPDATE topologies SET definition_json='{\"nodes\":[],\"edges\":[]}',\
                definition_digest=?2 WHERE id=?1",
            rusqlite::params![
                STARTERS[1].id.to_string(),
                crate::topology::store::digest("{\"nodes\":[],\"edges\":[]}"),
            ],
        )
        .unwrap();
    assert_eq!(
        starters::seed_all(&store)[1],
        (ROLLING_QA_SWEEP, SeedOutcome::Updated)
    );
    drop(store);
    assert_eq!(topology_row(&world, ROLLING_QA_SWEEP).await.0, 2);

    // An operator edit (the operator update path rewrites the text and leaves
    // the digest column) is never overwritten, however many boots pass.
    let edited = {
        let store = world.harness.executor.store.lock().await;
        let mut topology = starter_topology(ISSUE_IMPLEMENT_REVIEW_LAND);
        topology.definition.nodes[0].label = "Operator relabelled".into();
        topology.updated_at = chrono::Utc::now();
        store.update_topology(&topology).unwrap();
        serde_json::to_string(&topology.definition).unwrap()
    };
    for _ in 0..2 {
        let store = world.harness.executor.store.lock().await;
        assert_eq!(
            starters::seed_all(&store)[0],
            (ISSUE_IMPLEMENT_REVIEW_LAND, SeedOutcome::KeptOperatorEdit)
        );
    }
    let (revision, _, _, text) = topology_row(&world, ISSUE_IMPLEMENT_REVIEW_LAND).await;
    assert_eq!((revision, text), (1, edited));

    // An archived starter is not resurrected, and a foreign row holding the
    // name is left alone.
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE topologies SET archived_at='2026-10-07T00:00:00.000000000Z' WHERE id=?1",
            [STARTERS[1].id.to_string()],
        )
        .unwrap();
    assert_eq!(
        starters::seed_all(&store)[1],
        (ROLLING_QA_SWEEP, SeedOutcome::KeptArchived)
    );
    store
        .conn
        .execute(
            "UPDATE topologies SET id=?2 WHERE id=?1",
            rusqlite::params![STARTERS[0].id.to_string(), Uuid::new_v4().to_string()],
        )
        .unwrap();
    assert_eq!(
        starters::seed_all(&store)[0],
        (ISSUE_IMPLEMENT_REVIEW_LAND, SeedOutcome::NameTaken)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn starter_a_passes_agent_policy_check_with_grant_triples() {
    for starter in &STARTERS {
        let checked = agent::check_definition(&starter_topology(starter.name), &live_grant(), 0);
        assert!(checked.structural.is_empty(), "{:?}", checked.structural);
        assert!(checked.policy.is_empty(), "{:?}", checked.policy);
        assert!(checked.workflow.is_some());
    }

    // The triples are the plan's: Sonnet high implements and fixes, Codex Sol
    // high reviews, Sonnet medium triages. Never Haiku.
    let a = starter_topology(ISSUE_IMPLEMENT_REVIEW_LAND);
    let node = |id: &str| a.definition.nodes.iter().find(|n| n.id == id).unwrap();
    let triple = |id: &str| {
        let launch = agent::node_launch(
            &crate::session::SessionManager::bridge_topology(&a)
                .unwrap()
                .nodes
                .into_iter()
                .find(|n| n.id == id)
                .unwrap(),
        )
        .unwrap();
        (launch.provider, launch.model, launch.effort.unwrap())
    };
    let sonnet_high = (
        SessionProvider::Claude,
        "claude-sonnet-5-5".to_owned(),
        "high".to_owned(),
    );
    assert_eq!(triple("implement"), sonnet_high);
    assert_eq!(triple("fix"), sonnet_high);
    let review = node("review").step().unwrap().unwrap();
    assert!(matches!(
        review,
        rsi_common::types::TopologyStep::Review { reviewer, max_rounds: 2, .. }
            if reviewer.provider == SessionProvider::Codex
                && reviewer.model == "gpt-6.1-sol"
                && reviewer.effort == "high"
    ));
    assert!(matches!(
        node("land").step().unwrap().unwrap(),
        rsi_common::types::TopologyStep::Land { accepted, test_filters }
            if accepted == "review" && test_filters.is_empty()
    ));
    let b = starter_topology(ROLLING_QA_SWEEP);
    assert_eq!(
        agent::node_launch(
            &crate::session::SessionManager::bridge_topology(&b)
                .unwrap()
                .nodes
                .into_iter()
                .find(|n| n.id == "triage")
                .unwrap()
        )
        .unwrap()
        .effort
        .as_deref(),
        Some("medium")
    );
    for starter in &STARTERS {
        assert!(
            !serde_json::to_string(&starter.definition().unwrap())
                .unwrap()
                .to_ascii_lowercase()
                .contains("haiku"),
            "{} names Haiku",
            starter.name
        );
    }

    // Ungranted reviewer: the check refuses naming the reviewer.
    let without_sol: Vec<_> = live_grant()
        .into_iter()
        .filter(|l| l.model != "gpt-6.1-sol")
        .collect();
    let refused = agent::check_definition(&a, &without_sol, 0);
    assert!(
        refused
            .policy
            .iter()
            .any(|d| d.contains("reviewer") && d.contains("allowed_launches")),
        "{:?}",
        refused.policy
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_input_is_snapshotted_at_accept() {
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    let (issue, other) = {
        let store = world.harness.executor.store.lock().await;
        starters::seed_all(&store);
        let new = |title: &str, body: &str| NewIssue {
            project_id: world.project,
            title: title.into(),
            body: body.into(),
            priority: None,
            labels: Vec::new(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        (
            store
                .create_issue(&new("Snapshot me", "The body at accept."))
                .unwrap(),
            store.create_issue(&new("Other", "")).unwrap(),
        )
    };
    let digest = {
        let store = world.harness.executor.store.lock().await;
        let text: String = store
            .conn
            .query_row(
                "SELECT definition_json FROM topologies WHERE id=?1",
                [STARTERS[0].id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        crate::topology::store::digest(&text)
    };
    let request = |inputs: serde_json::Value, key: &str| AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        topology_id: STARTERS[0].id,
        expected_digest: digest.clone(),
        epic_id: world.epic_a,
        inputs,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: key.into(),
    };

    let accepted = agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        world.lead_a,
        &request(
            serde_json::json!({"issue": issue.display_number, "note": "kept"}),
            "run-1",
        ),
    )
    .await
    .unwrap()
    .result;
    let stored = {
        let store = world.harness.executor.store.lock().await;
        rows::load_execution(&store, accepted.execution_id)
            .unwrap()
            .unwrap()
            .input
            .unwrap()
    };
    assert_eq!(
        stored["issue"],
        serde_json::json!({
            "display_number": issue.display_number,
            "title": "Snapshot me",
            "body": "The body at accept.",
            "project_id": world.project,
        })
    );
    assert_eq!(stored["note"], "kept");

    // Later edits to the Issue do not reach the running execution: the
    // snapshot is what the source node's prompt carries.
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE issues SET title='Edited after accept' WHERE id=?1",
                [issue.id.to_string()],
            )
            .unwrap();
    }
    assert_eq!(
        world
            .harness
            .executor
            .advance(accepted.execution_id)
            .await
            .unwrap(),
        Step::Wait
    );
    let implement = world
        .harness
        .attempt(accepted.execution_id, "implement", 0, 1)
        .await;
    assert!(
        implement
            .query()
            .contains("### issue\n#1 Snapshot me\n\nThe body at accept."),
        "{}",
        implement.query()
    );
    assert!(!implement.query().contains("Edited after accept"));
    assert!(!implement.query().contains("_rsi_on_call"));

    // An Issue that does not exist in the Epic's project is a stable refusal,
    // and nothing is persisted.
    let missing = agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        world.lead_a,
        &request(
            serde_json::json!({"issue": other.display_number + 40}),
            "run-2",
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(error_code(&missing), "issue_not_found");

    // Non-integer `issue` inputs keep their old meaning.
    let store = world.harness.executor.store.lock().await;
    let text = serde_json::json!({"issue": "free text"});
    assert_eq!(
        starters::resolve_issue_input(&store, Some(world.project), Some(text.clone())).unwrap(),
        Some(text)
    );
    // Without a project there is nothing to resolve a number in.
    assert!(
        starters::resolve_issue_input(&store, None, Some(serde_json::json!({"issue": 1}))).is_err()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn starter_b_files_triage_only_on_a_red_baseline() {
    let workflow =
        crate::session::SessionManager::bridge_topology(&starter_topology(ROLLING_QA_SWEEP))
            .unwrap();

    // Green: the gate is true, so the triage session never launches.
    let green = Harness::new();
    let execution = green.start(workflow.clone()).await;
    assert_eq!(green.run_to_end(execution).await, Step::Done);
    assert_eq!(green.status(execution).await, ExecutionStatus::Succeeded);
    let triage = green.attempt(execution, "triage", 0, 1).await;
    assert_eq!(triage.status, AttemptStatus::Skipped);
    assert!(green.launches().is_empty());

    // Red: the baseline fails, the gate is false, and triage runs on the typed
    // report with its instructions.
    let red = Harness::new();
    let execution = red.start(workflow).await;
    assert_eq!(red.executor.advance(execution).await.unwrap(), Step::Wait);
    let baseline = red.attempt(execution, "baseline", 0, 1).await;
    assert_eq!(baseline.status, AttemptStatus::Running);
    red.finish_command(baseline.id, 101);
    assert_eq!(red.run_to_end(execution).await, Step::Done);
    let triage = red.attempt(execution, "triage", 0, 1).await;
    assert_eq!(triage.status, AttemptStatus::Succeeded);
    let query = triage.query();
    assert!(query.contains("exit_code: 101"), "{query}");
    assert!(query.contains("AgentCreateIssue"), "{query}");
    assert!(query.contains("do not write qa-green.sha"), "{query}");
}
