//! #1641 S5a acceptance tests: seeded starter topologies and the Issue
//! snapshot input. A child module of `agent_tests`, so it shares its `World`.

use super::issue_close_tests::{issue_now, open_issue, status_events, topology_events};
use super::*;
use crate::topology::land::{LandAcceptance, LandStatus};
use crate::topology::starters::{
    self, ISSUE_IMPLEMENT_REVIEW_LAND, ISSUE_PIPELINE, ISSUE_PIPELINE_POST_REVIEW,
    ISSUE_PIPELINE_REVIEWED, ROLLING_QA_SWEEP, STARTERS, SeedOutcome,
};
use rsi_common::agent_jobs::JobState;
use rsi_common::types::{IssueStatus, NewIssue, TopologyStep};

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
pub(super) fn live_grant() -> Vec<ManagerLaunchChoiceV2> {
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
            (ISSUE_PIPELINE, SeedOutcome::Inserted),
            (ISSUE_PIPELINE_REVIEWED, SeedOutcome::Inserted),
            (ISSUE_PIPELINE_POST_REVIEW, SeedOutcome::Inserted),
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
            "UPDATE topologies SET id=?2,definition_json='{\"nodes\":[],\"edges\":[]}' WHERE id=?1",
            rusqlite::params![STARTERS[0].id.to_string(), Uuid::new_v4().to_string()],
        )
        .unwrap();
    assert_eq!(
        starters::seed_all(&store)[0],
        (ISSUE_IMPLEMENT_REVIEW_LAND, SeedOutcome::NameTaken)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
/// A legacy operator starter with a random id and text in struct (not sorted)
/// key order. Only the operator namespace is eligible for seed refresh.
fn operator_row_with(
    store: &crate::store::Store,
    name: &str,
    mutate: impl FnOnce(&mut Topology),
) -> (Uuid, String) {
    let mut topology = starter_topology(name);
    mutate(&mut topology);
    let text = serde_json::to_string(&topology.definition).unwrap();
    let id = Uuid::new_v4();
    store
        .conn
        .execute(
            "INSERT INTO topologies (id,name,definition_json,created_at,updated_at,owner_kind,\
                owner_session_id,project_id,epic_id,revision,definition_digest,shared) \
             VALUES (?1,?2,?3,'2026-10-13T00:00:00Z','2026-10-13T00:00:00Z','operator',NULL,NULL,\
                NULL,1,?4,1)",
            rusqlite::params![
                id.to_string(),
                name,
                text,
                crate::topology::store::digest(&text)
            ],
        )
        .unwrap();
    (id, text)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
fn relabel(topology: &mut Topology) {
    topology.definition.nodes[0].label = "An older shipped label".into();
}

/// #2660: a row holding a definition the daemon shipped earlier follows the
/// shipped one, however it was written; any other row is kept and flagged.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_starter_holding_an_older_shipped_digest_is_upgraded_and_an_edit_is_flagged() {
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    let store = world.harness.executor.store.lock().await;
    let starter = starter_named(ISSUE_PIPELINE);
    let current = crate::topology::store::digest(&starter.canonical_text().unwrap());
    let (id, old_text) = operator_row_with(&store, ISSUE_PIPELINE, relabel);
    let old_digest = starters::canonical_digest(&old_text).unwrap();
    assert_ne!(old_digest, current);
    let row = |store: &crate::store::Store| -> (i64, String, String, String) {
        store
            .conn
            .query_row(
                "SELECT revision,owner_kind,definition_json,definition_digest FROM topologies \
                 WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
    };

    // Its digest is not one the daemon shipped: kept, flagged, text untouched.
    assert_eq!(
        starters::seed_one(&store, starter, &[]).unwrap(),
        SeedOutcome::NameTaken
    );
    assert_eq!(row(&store).2, old_text);
    let flagged = starters::drift(ISSUE_PIPELINE, &old_text).expect("drift is reported");
    assert_eq!(flagged.shipped_digest, current);

    // Its digest is one the daemon shipped earlier: it becomes a new revision
    // of the same row holding the current definition.
    assert_eq!(
        starters::seed_one(&store, starter, &[old_digest.as_str()]).unwrap(),
        SeedOutcome::Updated
    );
    let (revision, owner, text, digest) = row(&store);
    assert_eq!((revision, owner.as_str()), (2, "operator"));
    assert_eq!(text, starter.canonical_text().unwrap());
    assert_eq!(digest, current);
    assert!(starters::drift(ISSUE_PIPELINE, &text).is_none());
    // The next boot has nothing left to do.
    assert_eq!(
        starters::seed_one(&store, starter, &[old_digest.as_str()]).unwrap(),
        SeedOutcome::Unchanged
    );

    // A fresh DB seeds the current definition and nothing is flagged.
    let fresh = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    let fresh_store = fresh.harness.executor.store.lock().await;
    starters::seed_all(&fresh_store);
    let listed = agent::list(
        &fresh_store,
        fresh.lead_a,
        &AgentTopologyListRequestV1::default(),
    )
    .unwrap()
    .topologies;
    assert!(listed.iter().any(|t| t.name == ISSUE_PIPELINE));
    assert!(
        listed.iter().all(|t| t.starter_drift.is_none()),
        "{listed:?}"
    );
}

/// #2693: one definition has one digest. A manager upsert of the shipped text
/// reports the same digest from `validate_only`, the real upsert, the stored
/// column, `AgentTopologyList` and the shipped pin, whatever order the
/// definition's `params` keys arrive in. A later seed creates the shared
/// fixed-ID starter and leaves the manager's private revision untouched.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_manager_upsert_of_shipped_text_has_one_digest_and_is_preserved_at_boot() {
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    let starter = starter_named(ISSUE_PIPELINE);
    let current = crate::topology::store::digest(&starter.canonical_text().unwrap());
    // Version N: the shipped text with its first label changed, standing in
    // for a text the daemon shipped before the current one.
    let mut older = starter.definition().unwrap();
    older.nodes[0].label = "An older shipped label".into();
    let older_text = serde_json::to_string(&older).unwrap();
    // Every read of the text builds a fresh `HashMap`, so key order differs.
    let reparsed = || -> TopologyDefinition { serde_json::from_str(&older_text).unwrap() };

    let mut validated = std::collections::HashSet::new();
    for attempt in 0..8 {
        let mut request = manager_upsert(ISSUE_PIPELINE, reparsed());
        request.validate_only = true;
        request.idempotency_key = format!("validate-{attempt}");
        validated.insert(
            world
                .upsert(world.manager, &request)
                .await
                .unwrap()
                .definition_digest,
        );
    }
    assert_eq!(
        validated.len(),
        1,
        "validate_only is order-stable: {validated:?}"
    );
    let validated = validated.into_iter().next().unwrap();
    assert_eq!(
        validated,
        starters::canonical_digest(&older_text).unwrap(),
        "validate_only digests the canonical text"
    );

    let mut request = manager_upsert(ISSUE_PIPELINE, reparsed());
    request.idempotency_key = "upsert-older".into();
    let upserted = world.upsert(world.manager, &request).await.unwrap();
    assert_eq!(upserted.definition_digest, validated);
    let topology_id = upserted.topology_id.unwrap();

    let store = world.harness.executor.store.lock().await;
    let (stored_text, column): (String, String) = store
        .conn
        .query_row(
            "SELECT definition_json,definition_digest FROM topologies WHERE id=?1",
            [topology_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(column, validated, "the stored column");
    assert_eq!(crate::topology::store::digest(&stored_text), validated);
    let listed = agent::list(
        &store,
        world.manager,
        &AgentTopologyListRequestV1::default(),
    )
    .unwrap()
    .topologies;
    let row = listed
        .iter()
        .find(|t| t.topology_id == topology_id)
        .unwrap();
    assert_eq!(row.definition_digest, validated, "AgentTopologyList");
    assert_eq!(
        row.starter_drift
            .as_ref()
            .map(|d| d.shipped_digest.as_str()),
        Some(current.as_str()),
        "N differs from the current text, so it is flagged against it"
    );

    // The daemon now ships N+1: publish it at the fixed ID while preserving
    // the manager's private definition, including its digest and revision.
    assert_eq!(
        starters::seed_one(&store, starter, &[validated.as_str()]).unwrap(),
        SeedOutcome::Inserted
    );
    let (revision, owner, upgraded, column): (i64, String, String, String) = store
        .conn
        .query_row(
            "SELECT revision,owner_kind,definition_json,definition_digest FROM topologies \
             WHERE id=?1",
            [topology_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((revision, owner.as_str()), (1, "manager"));
    assert_eq!(upgraded, stored_text);
    assert_eq!(column, validated);
    let listed = agent::list(
        &store,
        world.manager,
        &AgentTopologyListRequestV1::default(),
    )
    .unwrap()
    .topologies;
    let row = listed
        .iter()
        .find(|t| t.topology_id == topology_id)
        .unwrap();
    assert_eq!(row.definition_digest, validated);
    assert_eq!(row.starter_drift.as_ref().unwrap().shipped_digest, current);
    let shared = agent::get(
        &store,
        world.manager,
        &rsi_common::topology_agent::AgentTopologyGetRequestV1 {
            topology_id: Some(starter.id),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(shared.topology.definition_digest, current);
    assert_eq!(shared.topology.owner_kind, "operator");
    assert!(shared.topology.shared);

    // The shipped text itself, upserted by a manager: the same digest as the
    // shipped pin, and no drift to report.
    drop(store);
    let mut request = manager_upsert(
        ISSUE_PIPELINE_REVIEWED,
        reparsed_starter(ISSUE_PIPELINE_REVIEWED),
    );
    request.validate_only = true;
    request.idempotency_key = "validate-shipped".into();
    let shipped_digest = crate::topology::store::digest(
        &starter_named(ISSUE_PIPELINE_REVIEWED)
            .canonical_text()
            .unwrap(),
    );
    assert_eq!(
        world
            .upsert(world.manager, &request)
            .await
            .unwrap()
            .definition_digest,
        shipped_digest
    );
}

/// The starter's bundled JSON decoded afresh (a new `HashMap` per node).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
fn reparsed_starter(name: &str) -> TopologyDefinition {
    starter_named(name).definition().unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_edited_starter_stays_and_agent_topology_list_names_the_drift() {
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    let store = world.harness.executor.store.lock().await;
    starters::seed_all(&store);
    let mut edited = starter_topology(ISSUE_PIPELINE_REVIEWED);
    edited.definition.nodes[0].label = "Edited by a manager".into();
    edited.updated_at = chrono::Utc::now();
    store.update_topology(&edited).unwrap();
    // A boot leaves the edit in place.
    let outcomes = starters::seed_all(&store);
    assert!(outcomes.contains(&(ISSUE_PIPELINE_REVIEWED, SeedOutcome::KeptOperatorEdit)));
    let listed = agent::list(&store, world.lead_a, &AgentTopologyListRequestV1::default())
        .unwrap()
        .topologies;
    let shipped = crate::topology::store::digest(
        &starter_named(ISSUE_PIPELINE_REVIEWED)
            .canonical_text()
            .unwrap(),
    );
    for topology in &listed {
        if topology.name == ISSUE_PIPELINE_REVIEWED {
            assert_eq!(
                topology
                    .starter_drift
                    .as_ref()
                    .map(|d| d.shipped_digest.as_str()),
                Some(shipped.as_str())
            );
        } else {
            assert!(topology.starter_drift.is_none(), "{}", topology.name);
        }
    }
    // The flag survives a JSON round trip and is absent on a clean row.
    let json = serde_json::to_value(&listed).unwrap();
    assert!(json.to_string().contains("starter_drift"));
}

/// Every bundled starter survives a private manager-name collision. Exercise
/// real upsert, seeding, List/Get and Execute admission, without providers.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn private_starter_names_preserve_manager_rows_and_publish_executable_fixed_ids() {
    use rsi_common::topology_agent::AgentTopologyGetRequestV1;

    for conflict in [false, true] {
        let mut world =
            World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
        // rolling-qa-sweep admits real workspace members through cargo metadata.
        // The fixture has no dependencies and never compiles these packages.
        std::fs::write(
            world.harness.repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"rsi-common\", \"rsid\", \"rsi\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        for name in ["rsi-common", "rsid", "rsi"] {
            let package = world.harness.repo.join(name);
            std::fs::create_dir_all(package.join("src")).unwrap();
            std::fs::write(
                package.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
            )
            .unwrap();
            std::fs::write(package.join("src/lib.rs"), "").unwrap();
        }
        git(
            &world.harness.repo,
            &["add", "Cargo.toml", "rsi-common", "rsid", "rsi"],
        );
        git(
            &world.harness.repo,
            &["commit", "-q", "-m", "workspace fixture"],
        );
        world.harness.base = git(&world.harness.repo, &["rev-parse", "HEAD"]);
        let mut private = Vec::new();
        if conflict {
            for starter in &STARTERS {
                let mut definition = starter.definition().unwrap();
                definition.nodes[0].label = "Project manager's private revision".into();
                let row = world
                    .upsert(world.manager, &manager_upsert(starter.name, definition))
                    .await
                    .unwrap();
                private.push(row);
            }
        }
        let snapshot = |store: &crate::store::Store| -> Vec<String> {
            store.conn.prepare(
                "SELECT json_array(rowid,id,name,definition_json,created_at,updated_at,
                    owner_kind,owner_session_id,project_id,epic_id,revision,definition_digest,shared,archived_at)
                 FROM topologies WHERE owner_kind='manager' ORDER BY id"
            ).unwrap().query_map([], |row| row.get(0)).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        {
            let store = world.harness.executor.store.lock().await;
            let before = snapshot(&store);
            assert!(
                starters::seed_all(&store)
                    .iter()
                    .all(|(_, outcome)| *outcome == SeedOutcome::Inserted)
            );
            assert!(
                starters::seed_all(&store)
                    .iter()
                    .all(|(_, outcome)| *outcome == SeedOutcome::Unchanged)
            );
            assert_eq!(snapshot(&store), before);
            let listed = agent::list(&store, world.lead_a, &Default::default()).unwrap();
            assert_eq!(listed.topologies.len(), STARTERS.len());
            // A one-name cursor remains complete even when each visible name
            // has two underlying rows in the manager's scope.
            let mut page = AgentTopologyListRequestV1 {
                limit: Some(1),
                ..Default::default()
            };
            let mut paged = Vec::new();
            loop {
                let result = agent::list(&store, world.manager, &page).unwrap();
                paged.extend(result.topologies.into_iter().map(|row| row.topology_id));
                let Some(cursor) = result.next_cursor else {
                    break;
                };
                page.cursor = Some(cursor);
            }
            let expected: std::collections::HashSet<_> = if conflict {
                private.iter().map(|row| row.topology_id.unwrap()).collect()
            } else {
                STARTERS.iter().map(|starter| starter.id).collect()
            };
            assert_eq!(paged.len(), expected.len());
            assert_eq!(
                paged.into_iter().collect::<std::collections::HashSet<_>>(),
                expected
            );
            for (index, starter) in STARTERS.iter().enumerate() {
                let by_name = AgentTopologyGetRequestV1 {
                    name: Some(starter.name.into()),
                    ..Default::default()
                };
                let by_id = AgentTopologyGetRequestV1 {
                    topology_id: Some(starter.id),
                    ..Default::default()
                };
                let lead = agent::get(&store, world.lead_a, &by_name).unwrap();
                let fixed = agent::get(&store, world.lead_a, &by_id).unwrap();
                let manager_fixed = agent::get(&store, world.manager, &by_id).unwrap();
                assert_eq!(lead, fixed);
                assert_eq!(fixed, manager_fixed);
                assert_eq!(fixed.topology.topology_id, starter.id);
                assert_eq!(fixed.topology.revision, 1);
                assert_eq!(fixed.topology.owner_kind, "operator");
                assert!(fixed.topology.shared);
                assert_eq!(
                    fixed.definition,
                    serde_json::to_value(starter.definition().unwrap()).unwrap()
                );
                assert!(listed.topologies.iter().any(|row| row == &fixed.topology));
                let manager = agent::get(&store, world.manager, &by_name).unwrap();
                assert_eq!(
                    manager.topology.topology_id,
                    if conflict {
                        private[index].topology_id.unwrap()
                    } else {
                        starter.id
                    }
                );
            }
        }
        // Both callers execute the same canonical revision and digest. Each
        // run has its own Issue to respect the one-live-run-per-Issue guard.
        for starter in &STARTERS {
            for caller in [world.lead_a, world.manager] {
                let issue = open_issue(&world, "Starter collision admission").await;
                let digest = crate::topology::store::digest(&starter.canonical_text().unwrap());
                let accepted = agent::execute(
                    &world.harness.executor.store,
                    || KNOBS,
                    caller,
                    &AgentTopologyExecuteRequestV1 {
                        project_id: None,
                        on_call: None,
                        supersedes_execution: None,
                        wake: None,
                        topology_id: starter.id,
                        expected_digest: digest.clone(),
                        epic_id: world.epic_a,
                        inputs: serde_json::json!({"issue": issue.display_number}),
                        base_commit: Some(world.harness.base.clone()),
                        idempotency_key: format!("{}-{caller}", starter.name),
                    },
                )
                .await
                .unwrap();
                let store = world.harness.executor.store.lock().await;
                let execution = rows::load_execution(&store, accepted.result.execution_id)
                    .unwrap()
                    .unwrap();
                assert_eq!(execution.topology_id, Some(starter.id));
                assert_eq!(execution.topology_revision, Some(1));
                assert_eq!(execution.name, starter.name);
            }
        }
    }
}

/// #2660: `SHIPPED_BEFORE` holds, per starter, the digest each earlier shipped
/// text had, never the current one, without duplicates. The history test below
/// checks that every replaced definition is included.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn shipped_before_lists_the_replaced_digests_and_never_the_current_one() {
    for starter in &STARTERS {
        let current = crate::topology::store::digest(&starter.canonical_text().unwrap());
        let history = starters::SHIPPED_BEFORE
            .iter()
            .find(|(name, _)| *name == starter.name)
            .unwrap_or_else(|| panic!("{} has a history entry", starter.name))
            .1;
        assert!(!history.contains(&current.as_str()), "{}", starter.name);
        let unique: std::collections::HashSet<_> = history.iter().collect();
        assert_eq!(unique.len(), history.len(), "{}", starter.name);
    }
}

/// #2702: the canonical digest of a starter's JSON at every commit git history
/// shows changed it. `None` when this checkout has no usable history (a source
/// archive, a shallow clone without the commits), so the check below skips
/// rather than fails there.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
fn starter_history_digests(starter_name: &str) -> Option<Vec<(String, String)>> {
    let dir = rsi_common::manifest_dir!();
    let git = |args: &[&str]| -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let file = format!("src/topology/starters/{starter_name}.json");
    let log = git(&["log", "--format=%H", "HEAD", "--", &file])?;
    let mut digests = Vec::new();
    for commit in log.lines() {
        let text = git(&["show", &format!("{commit}:./{file}")])?;
        let digest = starters::canonical_digest(&text)
            .unwrap_or_else(|| panic!("{starter_name} at {commit} is not a definition"));
        digests.push((commit.to_owned(), digest));
    }
    Some(digests)
}

/// #2702: every `SHIPPED_BEFORE` pin is the canonical digest of a definition git
/// history shows the daemon shipped, and every earlier shipped definition is
/// pinned, so a manager copy at any earlier version is upgraded at boot and never
/// reads as `starter_drift`. Two changes that land in parallel (#2688 appended a
/// pin under the old digest scheme while #2693 converted the others) cannot leave
/// a pin wrong or missing unseen.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn shipped_before_pins_are_the_canonical_digests_git_history_shows() {
    let mut checked = 0;
    for starter in &STARTERS {
        let Some(history) = starter_history_digests(starter.name) else {
            eprintln!("no git history for {}: pins not recomputed", starter.name);
            continue;
        };
        let current = crate::topology::store::digest(&starter.canonical_text().unwrap());
        let pins = starters::SHIPPED_BEFORE
            .iter()
            .find(|(name, _)| *name == starter.name)
            .unwrap()
            .1;
        for (commit, digest) in &history {
            assert!(
                *digest == current || pins.contains(&digest.as_str()),
                "{} at {commit} shipped {digest}, which is neither the current \
                 definition nor in SHIPPED_BEFORE",
                starter.name
            );
        }
        for pin in pins {
            assert!(
                history.iter().any(|(_, digest)| digest == pin),
                "{} pins {pin}, which no commit of its history shipped (a pin \
                 taken under another digest scheme?)",
                starter.name
            );
        }
        checked += 1;
    }
    // A checkout with history checks every starter; one without checks none.
    assert!(checked == 0 || checked == STARTERS.len(), "{checked}");
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
                && reviewer.model == "gpt-6-luna"
                && reviewer.effort == "high"
    ));
    assert!(matches!(
        node("land").step().unwrap().unwrap(),
        rsi_common::types::TopologyStep::Land { accepted, test_filters, close_issue: false }
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
    let without_luna: Vec<_> = live_grant()
        .into_iter()
        .filter(|l| l.model != "gpt-6-luna")
        .collect();
    let refused = agent::check_definition(&a, &without_luna, 0);
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
        supersedes_execution: None,
        wake: None,
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
            .contains("> #1 Snapshot me\n> \n> The body at accept."),
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

/// #1775: every handoff the starter (a) prompts ask a node to write parses
/// under the executor's v2 parser, so a node that follows its prompt is never
/// failed `handoff_invalid`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn starter_a_prompted_handoffs_parse_for_the_executor() {
    let starter = STARTERS
        .iter()
        .find(|starter| starter.name == ISSUE_IMPLEMENT_REVIEW_LAND)
        .expect("starter a");
    let definition: serde_json::Value =
        serde_json::from_str(&starter.canonical_text().expect("text")).expect("json");
    let mut prompted = 0;
    for node in definition["nodes"].as_array().expect("nodes") {
        let Some(instructions) = node["params"]["instructions"].as_str() else {
            continue;
        };
        assert!(
            instructions.contains("doc_path: inline, manifest_path: inline, status: complete"),
            "node {} prompt names the complete handoff fields",
            node["id"]
        );
        prompted += 1;
    }
    assert_eq!(prompted, 2, "implement and fix prompts");
    let complete = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
                    doc_path: inline\n\
                    manifest_path: inline\n\
                    status: complete\n\
                    summary: done\n";
    let parsed = rsi_common::agent_contract::parse_pipeline_handoff_v2(complete, "")
        .expect("complete handoff parses");
    assert_eq!(
        parsed.strict_status,
        rsi_common::agent_contract::PipelineStatusV2::Complete
    );
    let blocked = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
                   doc_path: inline\n\
                   status: blocked\n\
                   blocker: product choice\n\
                   blocker_class: technical_impasse\n\
                   blocker_evidence: none yet\n\
                   blocker_question: A or B?\n";
    let parsed = rsi_common::agent_contract::parse_pipeline_handoff_v2(blocked, "")
        .expect("blocked handoff parses");
    assert_eq!(parsed.blocker_question.as_deref(), Some("A or B?"));
}

// Shared by the optional-field regression tests; keep current pins in one place.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
pub(super) const EXISTING_STARTER_DIGESTS: &[(&str, &str)] = &[
    (
        ISSUE_IMPLEMENT_REVIEW_LAND,
        "sha256:960c08847f32fada6c1a72d68aaf5b11c6a00f3cd39b9f013e4bb95e4be76d51",
    ),
    (
        ROLLING_QA_SWEEP,
        "sha256:604cd5a81ca90c4ef7d507a34c3777dc41170d4fa279219a0c2bd9fb0d78849b",
    ),
];

/// #2249 added the optional `require_verified` session field. The seeded
/// starters do not use it, so their canonical text and digest are byte-identical
/// to what rolling shipped: an unedited seeded row is never rewritten, and no
/// operator edit is mistaken for one. The digests are the `definition_digest`
/// values the daemon stored when it seeded these revisions (starter a rev 3,
/// starter b rev 1), read before the field existed. #2266 corrected the
/// `rolling-qa-sweep` triage handoff stage, so that starter's pin is the
/// digest of the corrected text.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn existing_starter_canonical_text_and_digest_unchanged() {
    for &(name, expected) in EXISTING_STARTER_DIGESTS {
        let starter = STARTERS
            .iter()
            .find(|starter| starter.name == name)
            .expect("starter");
        let text = starter.canonical_text().expect("text");
        assert_eq!(crate::topology::store::digest(&text), expected, "{name}");
        assert!(!text.contains("require_verified"), "{name}");
        // The snapshot an execution is accepted with re-serializes each step
        // through the typed enum: it must not grow the new key either.
        for node in &starter.definition().expect("definition").nodes {
            let step = node.step().expect("step");
            if let Some(step) = step {
                let json = serde_json::to_string(&step).expect("step json");
                assert!(!json.contains("require_verified"), "{name}: {json}");
            }
        }
    }
}

// ─── `issue-pipeline` (#2304 slice 1) ──────────────────────────────────────

/// The pinned `definition_digest` of the bundled `issue-pipeline` revision 1.
/// A change to its text moves this on purpose: an unedited seeded row then
/// advances, and an operator-edited one is left alone (see the seed test).
const ISSUE_PIPELINE_DIGEST: &str =
    "sha256:4d63a05f378390ad5ea1a785891d5653148fd5fff891e3056d28fe0f22704ef7";

/// #2688: the pre-land review a refused land routes to is the review node of
/// `issue-pipeline-reviewed`: no land but `pre_land` is admitted by it, the
/// only way in is the `review_required` edge, and nothing else loops.
fn assert_pre_land_review_route(definition: &rsi_common::types::TopologyDefinition, land: &str) {
    let reviews: Vec<_> = definition
        .nodes
        .iter()
        .filter(|node| matches!(node.step(), Ok(Some(TopologyStep::Review { .. }))))
        .map(|node| node.id.as_str())
        .filter(|id| id.starts_with("pre_"))
        .collect();
    assert_eq!(reviews, ["pre_review"]);
    let reviewed = starter_named(ISSUE_PIPELINE_REVIEWED).definition().unwrap();
    let node = |definition: &rsi_common::types::TopologyDefinition, id: &str| {
        serde_json::to_value(definition.nodes.iter().find(|n| n.id == id).unwrap()).unwrap()
    };
    // The same review launch, rounds and prompts as the reviewed route.
    let (pre, review) = (node(definition, "pre_review"), node(&reviewed, "review"));
    assert_eq!(pre["params"]["step"], review["params"]["step"]);
    let (pre, fix) = (node(definition, "pre_fix"), node(&reviewed, "fix"));
    assert_eq!(pre["params"]["instructions"], fix["params"]["instructions"]);
    assert_eq!(pre["params"]["step"], fix["params"]["step"]);
    assert!(matches!(
        node_step(definition, "pre_review"),
        TopologyStep::Review { of, max_rounds: 2, .. } if of == "implement"
    ));
    assert!(matches!(
        node_step(definition, land),
        TopologyStep::Land { accepted, close_issue: true, .. } if accepted == "pre_review"
    ));
    let when = |id: &str| {
        serde_json::to_value(
            definition
                .nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap()
                .incoming_when()
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        when("pre_review"),
        serde_json::json!({"land": "review_required"})
    );
    assert_eq!(
        when("pre_fix"),
        serde_json::json!({"pre_review": "verdict_changes_requested"})
    );
    assert_eq!(
        when(land),
        serde_json::json!({"pre_review": "verdict_accepted"})
    );
    let loops: Vec<_> = definition
        .edges
        .iter()
        .filter(|edge| edge.loop_edge)
        .map(|edge| (edge.from.as_str(), edge.to.as_str()))
        .collect();
    // #2699: the repaired head has its own review route, and these two fix
    // loops are the only cycles.
    assert_eq!(
        loops,
        [("pre_fix", "pre_review"), ("repair_fix", "repair_review")]
    );
    assert_eq!(
        serde_json::to_value(&definition.until).unwrap(),
        serde_json::to_value(&reviewed.until).unwrap(),
        "the fix loops are bounded like the reviewed route"
    );
    assert_repair_review_route(definition);
}

/// #2699: `land_repair` has the same `review_required` route as `land`, for the
/// exact repaired head: `repair_review` reviews `repair` with the launch,
/// rounds and fix prompt of `pre_review`/`pre_fix`, and only `repair_land`
/// publishes what it accepted.
fn assert_repair_review_route(definition: &rsi_common::types::TopologyDefinition) {
    let node = |id: &str| {
        serde_json::to_value(definition.nodes.iter().find(|n| n.id == id).unwrap()).unwrap()
    };
    let (pre, repair) = (node("pre_review"), node("repair_review"));
    let mut expected = pre["params"]["step"].clone();
    expected["of"] = "repair".into();
    assert_eq!(repair["params"]["step"], expected);
    let (pre, fix) = (node("pre_fix"), node("repair_fix"));
    assert_eq!(pre["params"]["instructions"], fix["params"]["instructions"]);
    assert_eq!(pre["params"]["step"], fix["params"]["step"]);
    assert!(matches!(
        node_step(definition, "repair_land"),
        TopologyStep::Land { accepted, close_issue: true, .. } if accepted == "repair_review"
    ));
    let when = |id: &str| {
        serde_json::to_value(
            definition
                .nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap()
                .incoming_when()
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        when("repair_review"),
        serde_json::json!({"land_repair": "review_required"})
    );
    assert_eq!(
        when("repair_fix"),
        serde_json::json!({"repair_review": "verdict_changes_requested"})
    );
    assert_eq!(
        when("repair_land"),
        serde_json::json!({"repair_review": "verdict_accepted"})
    );
}

/// #2892: `repair` -> `land_repair` -> `repair_2` -> `land_repair_2` ->
/// `repair_3` -> `land_repair_3`, each repair entered only by the previous
/// land's failure, each land landing its own repair; the chain ends there.
/// (A `post_review*` edge out of a land is the post-land route's, #2899.)
fn assert_repair_chain(definition: &rsi_common::types::TopologyDefinition) {
    let when = |id: &str| {
        serde_json::to_value(
            definition
                .nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap()
                .incoming_when()
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(when("repair"), serde_json::json!({"land": "failure"}));
    assert_eq!(
        when("repair_2"),
        serde_json::json!({"land_repair": "failure"})
    );
    assert_eq!(
        when("repair_3"),
        serde_json::json!({"land_repair_2": "failure"})
    );
    let out_of = |id: &str| -> Vec<&str> {
        definition
            .edges
            .iter()
            .filter(|edge| edge.from == id && !edge.to.starts_with("post_review"))
            .map(|edge| edge.to.as_str())
            .collect()
    };
    assert_eq!(out_of("land_repair"), ["repair_2", "repair_review"]);
    assert_eq!(out_of("land_repair_2"), ["repair_3"]);
    assert!(out_of("land_repair_3").is_empty(), "the budget ends here");
    // Every repair prompt is told which attempt it is and to re-merge the
    // CURRENT origin/rolling right before it submits its verification.
    for (index, id) in ["repair", "repair_2", "repair_3"].into_iter().enumerate() {
        let node =
            serde_json::to_value(definition.nodes.iter().find(|n| n.id == id).unwrap()).unwrap();
        let instructions = node["params"]["instructions"].as_str().unwrap();
        assert!(
            instructions.starts_with(&format!("Repair attempt {} of 3.", index + 1)),
            "{id}: {instructions}"
        );
        for needle in [
            "merge the CURRENT origin/rolling as your last step before you submit the scoped_test job",
            "merge again and verify that final commit again",
            "needs a judgement",
        ] {
            assert!(instructions.contains(needle), "{id} lacks {needle:?}");
        }
    }
}

fn pipeline_starter() -> &'static starters::Starter {
    STARTERS
        .iter()
        .find(|starter| starter.name == ISSUE_PIPELINE)
        .expect("issue-pipeline is bundled")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn issue_pipeline_has_its_own_digest_fixture_and_the_default_route_shape() {
    let text = pipeline_starter().canonical_text().expect("text");
    assert_eq!(
        crate::topology::store::digest(&text),
        ISSUE_PIPELINE_DIGEST,
        "{text}"
    );
    // A fixed identity distinct from the older starters.
    assert_eq!(
        STARTERS
            .iter()
            .map(|starter| starter.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        STARTERS.len()
    );

    let definition = pipeline_starter().definition().expect("definition");
    let ids: Vec<_> = definition.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "implement",
            "land",
            "repair",
            "land_repair",
            "repair_2",
            "land_repair_2",
            "repair_3",
            "land_repair_3",
            "pre_review",
            "pre_fix",
            "pre_land",
            "repair_review",
            "repair_fix",
            "repair_land"
        ]
    );
    let step = |id: &str| {
        definition
            .nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap()
            .step()
            .unwrap()
            .unwrap()
    };
    // Both sessions are verified by their own final-HEAD receipt; both lands
    // derive their filters and close the Issue with the published SHA.
    for id in ["implement", "repair", "repair_2", "repair_3"] {
        assert!(
            matches!(
                step(id),
                TopologyStep::Session {
                    expects_commit: true,
                    require_verified: true,
                    ..
                }
            ),
            "{id}"
        );
    }
    for (id, accepted) in [
        ("land", "implement"),
        ("land_repair", "repair"),
        ("land_repair_2", "repair_2"),
        ("land_repair_3", "repair_3"),
    ] {
        assert!(
            matches!(
                step(id),
                TopologyStep::Land { accepted: a, test_filters, close_issue: true }
                    if a == accepted && test_filters.is_empty()
            ),
            "{id}"
        );
    }
    // #2688: the ordinary route has no reviewer on its path. Its only review
    // is `pre_review`, reached only through the `review_required` edge out of
    // `land`; the fix loops are the only cycles.
    assert_pre_land_review_route(&definition, "pre_land");
    // #2892: a refused land is repaired, and a repaired head the queue refuses
    // again is repaired again, in a chain bounded to three repairs (no cycle:
    // each is its own node).
    assert_repair_chain(&definition);

    // Sonnet high implements and repairs, as the other starters do.
    let sonnet_high = (
        SessionProvider::Claude,
        "claude-sonnet-5-5".to_owned(),
        "high".to_owned(),
    );
    let topology = starter_topology(ISSUE_PIPELINE);
    for id in ["implement", "repair", "repair_2", "repair_3"] {
        let launch = agent::node_launch(
            &crate::session::SessionManager::bridge_topology(&topology)
                .unwrap()
                .nodes
                .into_iter()
                .find(|n| n.id == id)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (launch.provider, launch.model, launch.effort.unwrap()),
            sonnet_high,
            "{id}"
        );
    }

    // The prompts carry the verification contract word for word.
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    for id in ["implement", "repair", "repair_2", "repair_3"] {
        let node = value["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == id)
            .unwrap();
        let instructions = node["params"]["instructions"].as_str().unwrap();
        for needle in [
            "ONE job",
            "\"wake\":\"none\"",
            "ONE AgentScheduleWake",
            "jobs_terminal",
            "end your turn without a handoff",
            "never commit after your last verification",
            "doc_path: inline, manifest_path: inline, status: complete",
        ] {
            assert!(
                instructions.to_lowercase().contains(&needle.to_lowercase()),
                "{id} prompt lacks {needle:?}"
            );
        }
    }
    let implement = value["nodes"][0]["params"]["instructions"]
        .as_str()
        .unwrap();
    assert!(implement.contains("Do not push and do not touch rolling or main"));
    assert!(implement.contains("actual numeric exit code"));
    assert!(implement.contains("exact head your scoped_test receipt verified"));
    // The handoffs it prompts parse for the executor.
    let complete = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
                    doc_path: inline\nmanifest_path: inline\nstatus: complete\nsummary: done\n";
    rsi_common::agent_contract::parse_pipeline_handoff_v2(complete, "").expect("implementation");
    let repaired = "PIPELINE HANDOFF — FIX:\n\
                    doc_path: inline\nmanifest_path: inline\nstatus: complete\nsummary: merged\n";
    rsi_common::agent_contract::parse_pipeline_handoff_v2(repaired, "").expect("repair");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_seed_is_idempotent_and_keeps_edited_or_archived_definitions() {
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    {
        let store = world.harness.executor.store.lock().await;
        let first = starters::seed_all(&store);
        assert!(
            first.contains(&(ISSUE_PIPELINE, SeedOutcome::Inserted)),
            "{first:?}"
        );
        for _ in 0..2 {
            assert!(
                starters::seed_all(&store)
                    .iter()
                    .all(|(_, outcome)| *outcome == SeedOutcome::Unchanged)
            );
        }
    }
    let (revision, owner_kind, shared, text) = topology_row(&world, ISSUE_PIPELINE).await;
    assert_eq!((revision, owner_kind.as_str(), shared), (1, "operator", 1));
    assert_eq!(text, pipeline_starter().canonical_text().unwrap());

    // An operator edit survives every later boot, byte for byte.
    let edited = {
        let store = world.harness.executor.store.lock().await;
        let mut topology = starter_topology(ISSUE_PIPELINE);
        topology.definition.nodes[0].label = "Operator relabelled".into();
        topology.updated_at = chrono::Utc::now();
        store.update_topology(&topology).unwrap();
        serde_json::to_string(&topology.definition).unwrap()
    };
    for _ in 0..2 {
        let store = world.harness.executor.store.lock().await;
        assert!(
            starters::seed_all(&store).contains(&(ISSUE_PIPELINE, SeedOutcome::KeptOperatorEdit))
        );
    }
    let (revision, _, _, text) = topology_row(&world, ISSUE_PIPELINE).await;
    assert_eq!((revision, text), (1, edited));

    // An archived definition is not resurrected, and its sibling seeds keep
    // their own state.
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE topologies SET archived_at='2026-10-15T00:00:00.000000000Z' WHERE id=?1",
            [pipeline_starter().id.to_string()],
        )
        .unwrap();
    let seeded = starters::seed_all(&store);
    assert!(
        seeded.contains(&(ISSUE_PIPELINE, SeedOutcome::KeptArchived)),
        "{seeded:?}"
    );
    assert!(seeded.contains(&(ISSUE_IMPLEMENT_REVIEW_LAND, SeedOutcome::Unchanged)));
    assert!(seeded.contains(&(ROLLING_QA_SWEEP, SeedOutcome::Unchanged)));
}

/// A project manager that may run topologies and coordinate Issues, with the
/// live launch grant the bundled starters are written against.
pub(super) fn pipeline_manager() -> ManagerPolicyV2 {
    policy(
        &[
            ManagerCapabilityV2::Automation,
            ManagerCapabilityV2::IssueCoordinate,
        ],
        live_grant(),
        64,
    )
}

/// Seed the bundled starters and execute the real `issue-pipeline` row on a
/// fresh Open Issue, exactly as `AgentTopologyExecute` would. The graph is the
/// bundled one: nothing here rebuilds it.
pub(super) async fn start_pipeline(
    world: &World,
    issue: &rsi_common::types::Issue,
    key: &str,
) -> Uuid {
    start_starter(world, pipeline_starter(), issue, key).await
}

/// [`start_pipeline`] for any bundled starter.
pub(super) async fn start_starter(
    world: &World,
    starter: &starters::Starter,
    issue: &rsi_common::types::Issue,
    key: &str,
) -> Uuid {
    let digest = {
        let store = world.harness.executor.store.lock().await;
        starters::seed_all(&store);
        let text: String = store
            .conn
            .query_row(
                "SELECT definition_json FROM topologies WHERE id=?1",
                [starter.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        crate::topology::store::digest(&text)
    };
    let request = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        supersedes_execution: None,
        wake: None,
        topology_id: starter.id,
        expected_digest: digest,
        epic_id: world.epic_a,
        inputs: serde_json::json!({"issue": issue.display_number}),
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
    .unwrap()
    .result
    .execution_id
}

/// Launch `node`, let its session commit, and record its green receipt for
/// exactly that commit. Returns the attempt, the commit and the job.
pub(super) async fn deliver_verified(
    world: &World,
    execution: Uuid,
    node: &str,
) -> (rows::AttemptRow, String, Uuid) {
    world.harness.executor.advance(execution).await.unwrap();
    let attempt = world.harness.attempt(execution, node, 0, 1).await;
    assert_eq!(attempt.status, AttemptStatus::Running, "{node}");
    let head = world
        .harness
        .commit_and_complete(attempt.session_id, &format!("{node}-work"));
    let job = scoped_test_job(
        &world.harness,
        attempt.session_id,
        JobState::Succeeded,
        green_receipt(&head),
    )
    .await;
    (attempt, head, job)
}

/// Advance until the land node `node` waits on the merge queue.
pub(super) async fn land_waiting_on(
    world: &World,
    execution: Uuid,
    node: &str,
) -> rows::AttemptRow {
    for _ in 0..6 {
        world.harness.executor.advance(execution).await.unwrap();
        if let Some(land) = world
            .harness
            .attempts(execution)
            .await
            .into_iter()
            .find(|attempt| attempt.node_id == node && attempt.status == AttemptStatus::Waiting)
        {
            return land;
        }
    }
    panic!("{node} never reached the queue");
}

pub(super) fn refuse(world: &World, land: &rows::AttemptRow) {
    world.harness.set_land_state(
        land.land_entry_id.unwrap(),
        LandStatus::Refused {
            state: "refused".into(),
            reason: "queue_batch_merge_conflict".into(),
        },
    );
}

async fn node_statuses(world: &World, execution: Uuid, nodes: &[&str]) -> Vec<AttemptStatus> {
    let attempts = world.harness.attempts(execution).await;
    nodes
        .iter()
        .map(|node| {
            attempts
                .iter()
                .find(|attempt| attempt.node_id == *node)
                .map_or(AttemptStatus::Skipped, |attempt| attempt.status)
        })
        .collect()
}

/// [`start_starter`] with explicit `inputs`, returning the admission result.
async fn execute_starter_with(
    world: &World,
    starter: &starters::Starter,
    inputs: serde_json::Value,
    key: &str,
) -> Result<Uuid> {
    let digest = {
        let store = world.harness.executor.store.lock().await;
        starters::seed_all(&store);
        let text: String = store
            .conn
            .query_row(
                "SELECT definition_json FROM topologies WHERE id=?1",
                [starter.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        crate::topology::store::digest(&text)
    };
    let request = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        supersedes_execution: None,
        wake: None,
        topology_id: starter.id,
        expected_digest: digest,
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

/// #2304 slice 3 (F-003): execute admission validates the chosen review policy
/// against the starter's graph and records it with the input snapshot; the
/// land request the daemon later enqueues carries what the graph covers.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_starters_record_and_enforce_the_review_policy() {
    use crate::topology::review_policy::ReviewPolicy;
    let world = World::new(pipeline_manager()).await;
    let recorded = |execution: Uuid| {
        let world = &world;
        async move {
            let store = world.harness.executor.store.lock().await;
            rows::load_execution(&store, execution)
                .unwrap()
                .unwrap()
                .input
                .unwrap()["review_policy"]
                .clone()
        }
    };

    // (starter, chosen value, what is recorded or the refusal code)
    let cases: [(&str, Option<&str>, std::result::Result<&str, &str>); 8] = [
        (ISSUE_PIPELINE, None, Ok("ordinary")),
        (ISSUE_PIPELINE, Some("ordinary"), Ok("ordinary")),
        (
            ISSUE_PIPELINE,
            Some("pre_merge"),
            Err("review_policy_graph_mismatch"),
        ),
        (
            ISSUE_PIPELINE,
            Some("post_land"),
            Err("review_policy_graph_mismatch"),
        ),
        (ISSUE_PIPELINE, Some("never"), Err("review_policy_invalid")),
        (ISSUE_PIPELINE_POST_REVIEW, None, Ok("post_land")),
        (ISSUE_PIPELINE_REVIEWED, None, Ok("pre_merge")),
        (ISSUE_PIPELINE_REVIEWED, Some("post_land"), Ok("post_land")),
    ];
    for (index, (name, chosen, expected)) in cases.into_iter().enumerate() {
        let issue = open_issue(&world, &format!("Policy case {index}")).await;
        let mut inputs = serde_json::json!({"issue": issue.display_number});
        if let Some(chosen) = chosen {
            inputs["review_policy"] = chosen.into();
        }
        let outcome = execute_starter_with(
            &world,
            starter_named(name),
            inputs,
            &format!("policy-{index}"),
        )
        .await;
        match expected {
            Ok(policy) => {
                let execution =
                    outcome.unwrap_or_else(|error| panic!("{name} {chosen:?}: {error}"));
                assert_eq!(recorded(execution).await, policy, "{name} {chosen:?}");
            }
            Err(code) => {
                let error = outcome
                    .err()
                    .unwrap_or_else(|| panic!("{name} {chosen:?} admitted"));
                assert_eq!(error_code(&error), code, "{name} {chosen:?}");
            }
        }
    }

    // The enqueue carries the cover of the land node it runs for.
    let issue = open_issue(&world, "Policy land request").await;
    let execution = start_starter(
        &world,
        starter_named(ISSUE_PIPELINE_POST_REVIEW),
        &issue,
        "policy-land",
    )
    .await;
    deliver_verified(&world, execution, "implement").await;
    land_waiting_on(&world, execution, "land").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1.review_coverage, Some(ReviewPolicy::PostLand));
}

/// F-001: an Open Issue reaches Closed with its published SHA after exactly
/// one implementation launch, with no repair and no manager effect between the
/// acceptance and the closure.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_bundled_graph_closes_after_publication() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Ordinary default route").await;
    assert_eq!(issue.status, IssueStatus::Open);
    let execution = start_pipeline(&world, &issue, "pipeline-ok").await;

    let (implement, head, job) = deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;

    // The queue is asked for exactly the verified commit, admitted by the
    // session's own receipt, with no hand-picked filters.
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0].1;
    assert_eq!(request.source_commit, head);
    assert_eq!(request.author_session_id, implement.session_id);
    assert!(request.test_filters.is_empty());
    assert_eq!(
        request.acceptance,
        LandAcceptance::Verified {
            node: "implement".into(),
            session_id: implement.session_id,
            job_id: job,
            head: head.clone(),
        }
    );
    // Queued is not landed: the Issue is still held by the run.
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );

    let landed_sha = "a".repeat(40);
    world.harness.set_land_state(
        land.land_entry_id.unwrap(),
        LandStatus::Published {
            landed_sha: Some(landed_sha.clone()),
        },
    );
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);

    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    let closed = issue_now(&world, &issue).await;
    assert_eq!(closed.status, IssueStatus::Closed);
    assert!(closed.closed_at.is_some());
    let land = world.harness.attempt(execution, "land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(
        land.output.as_ref().unwrap()["fields"]["issue_close"]["status"],
        "closed"
    );
    // The closing note names the published SHA and the verified commit.
    let notes = topology_events(&world, execution, "topology_landed").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["detail"]["landed_sha"], landed_sha);
    assert_eq!(notes[0]["detail"]["source_commit"], head);
    assert_eq!(notes[0]["detail"]["issue"], issue.display_number);

    // One implementation launch, no continuation, no repair, and the only
    // Issue writes are the executor's own start and land-close.
    assert_eq!(world.harness.launches().len(), 1);
    assert!(world.harness.continues().is_empty());
    assert_eq!(
        node_statuses(&world, execution, &["repair", "land_repair"]).await,
        vec![AttemptStatus::Skipped, AttemptStatus::Skipped]
    );
    assert_eq!(
        status_events(&world, &issue).await,
        vec![
            format!("topology-start:{execution}"),
            format!("topology-land:{execution}:{}", land.id)
        ]
    );
    assert!(
        topology_events(&world, execution, "issue_close_refused")
            .await
            .is_empty()
    );
}

/// F-004: one refusal takes exactly one verified repair and the repaired
/// commit closes the Issue; the repair is forked from the accepted commit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_one_refusal_takes_exactly_one_repair_and_closes() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Refused once").await;
    let execution = start_pipeline(&world, &issue, "pipeline-repair").await;

    let (_, head, _) = deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);

    // The refusal launches the repair session, which sees the queue's reason
    // and starts from the accepted commit.
    let (repair, repaired, job) = deliver_verified(&world, execution, "repair").await;
    assert!(
        repair.query().contains("queue_batch_merge_conflict"),
        "{}",
        repair.query()
    );
    let launches = world.harness.launches();
    assert_eq!(launches.len(), 2);
    assert_eq!(launches[1].2, head);
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );

    let land_repair = land_waiting_on(&world, execution, "land_repair").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].1.source_commit, repaired);
    assert!(requests[1].1.test_filters.is_empty());
    assert_eq!(
        requests[1].1.acceptance,
        LandAcceptance::Verified {
            node: "repair".into(),
            session_id: repair.session_id,
            job_id: job,
            head: repaired.clone(),
        }
    );
    let landed_sha = "b".repeat(40);
    world.harness.set_land_state(
        land_repair.land_entry_id.unwrap(),
        LandStatus::Published {
            landed_sha: Some(landed_sha.clone()),
        },
    );
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);

    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    assert_eq!(
        node_statuses(&world, execution, &["land", "repair", "land_repair"]).await,
        vec![
            AttemptStatus::Failed,
            AttemptStatus::Succeeded,
            AttemptStatus::Succeeded
        ]
    );
    assert_eq!(world.harness.launches().len(), 2, "exactly one repair");
    let notes = topology_events(&world, execution, "topology_landed").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["detail"]["landed_sha"], landed_sha);
    assert_eq!(notes[0]["detail"]["node_id"], "land_repair");
}

/// #2892 (3): two landings that conflict. The repaired head is refused again
/// because rolling advanced in the same file while it waited in the queue; the
/// executor repairs it a second time on its own (a new session forked from the
/// refused repair head, told it is attempt 2 of 3 and what the queue said),
/// and that head lands and closes the Issue. No manager turn, no relaunch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_second_conflict_is_repaired_again_and_lands() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Conflicts twice").await;
    let execution = start_pipeline(&world, &issue, "pipeline-conflict-twice").await;

    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    let (_, first_repair, _) = deliver_verified(&world, execution, "repair").await;
    let land_repair = land_waiting_on(&world, execution, "land_repair").await;
    // Rolling advanced in the same file while the repair waited.
    refuse(&world, &land_repair);

    let (repair_2, second_repair, job) = deliver_verified(&world, execution, "repair_2").await;
    assert_ne!(first_repair, second_repair);
    let query = repair_2.query();
    assert!(query.contains("Repair attempt 2 of 3."), "{query}");
    assert!(query.contains("queue_batch_merge_conflict"), "{query}");
    // implement, repair, repair_2: the second repair starts from the head the
    // queue refused, so the first repair's merge is not redone.
    let launches = world.harness.launches();
    assert_eq!(launches.len(), 3);
    assert_eq!(launches[2].2, first_repair);
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );

    let land_repair_2 = land_waiting_on(&world, execution, "land_repair_2").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].1.source_commit, second_repair);
    assert_eq!(
        requests[2].1.acceptance,
        LandAcceptance::Verified {
            node: "repair_2".into(),
            session_id: repair_2.session_id,
            job_id: job,
            head: second_repair.clone(),
        }
    );
    let landed_sha = "d".repeat(40);
    world.harness.set_land_state(
        land_repair_2.land_entry_id.unwrap(),
        LandStatus::Published {
            landed_sha: Some(landed_sha.clone()),
        },
    );
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);

    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    assert_eq!(
        node_statuses(
            &world,
            execution,
            &[
                "land",
                "repair",
                "land_repair",
                "repair_2",
                "land_repair_2",
                "repair_3",
                "land_repair_3"
            ]
        )
        .await,
        vec![
            AttemptStatus::Failed,
            AttemptStatus::Succeeded,
            AttemptStatus::Failed,
            AttemptStatus::Succeeded,
            AttemptStatus::Succeeded,
            AttemptStatus::Skipped,
            AttemptStatus::Skipped,
        ]
    );
    assert_eq!(world.harness.launches().len(), 3, "no third repair");
    let notes = topology_events(&world, execution, "topology_landed").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["detail"]["landed_sha"], landed_sha);
    assert_eq!(notes[0]["detail"]["source_commit"], second_repair);
    assert_eq!(notes[0]["detail"]["node_id"], "land_repair_2");
}

/// #3036: two pipelines whose changes conflict, the second queued behind the
/// first. The second land, refused `queue_batch_merge_conflict` while the
/// first is still gating, waits (no repair, the scan runs once and survives a
/// restart) until the first publishes; then exactly one repair runs and both
/// land and close their Issues.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_conflict_with_an_entry_ahead_waits_for_it_and_both_land_with_one_repair_3036() {
    let mut world = World::new(pipeline_manager()).await;
    let first_issue = open_issue(&world, "Untrack the build output").await;
    let second_issue = open_issue(&world, "Edit the build output").await;
    let first = start_pipeline(&world, &first_issue, "pipeline-3036-ahead").await;
    let second = start_pipeline(&world, &second_issue, "pipeline-3036-behind").await;
    deliver_verified(&world, first, "implement").await;
    let ahead = land_waiting_on(&world, first, "land").await;
    deliver_verified(&world, second, "implement").await;
    let behind = land_waiting_on(&world, second, "land").await;
    let (ahead_entry, behind_entry) = (ahead.land_entry_id.unwrap(), behind.land_entry_id.unwrap());
    world
        .harness
        .set_land_conflicts(behind_entry, vec![ahead_entry]);
    refuse(&world, &behind);

    for _ in 0..3 {
        world.harness.executor.advance(second).await.unwrap();
    }
    world.harness.restart();
    world.harness.executor.advance(second).await.unwrap();
    let held = world.harness.attempt(second, "land", 0, 1).await;
    assert_eq!(held.status, AttemptStatus::Waiting);
    assert_eq!(
        node_statuses(&world, second, &["repair"]).await,
        vec![AttemptStatus::Skipped]
    );
    assert_eq!(
        world.harness.launches().len(),
        2,
        "no repair while it waits"
    );
    assert_eq!(world.harness.land_conflict_scans(), vec![behind_entry]);
    let waits = topology_events(&world, second, rows::LAND_CONFLICT_WAIT).await;
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(waits[0]["detail"]["entry_id"], behind_entry.to_string());
    assert_eq!(
        waits[0]["detail"]["waiting_for"],
        serde_json::json!([ahead_entry.to_string()])
    );

    world.harness.set_land_state(
        ahead_entry,
        LandStatus::Published {
            landed_sha: Some("a".repeat(40)),
        },
    );
    assert_eq!(world.harness.run_to_end(first).await, Step::Done);
    let (repair, _, _) = deliver_verified(&world, second, "repair").await;
    assert!(
        repair.query().contains("Repair attempt 1 of 3."),
        "{}",
        repair.query()
    );
    let land = world.harness.attempt(second, "land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Failed);
    let error = land.error.unwrap();
    assert!(error.contains("queue_batch_merge_conflict"), "{error}");
    assert!(
        error.contains(&format!("{ahead_entry} published")),
        "{error}"
    );
    let land_repair = land_waiting_on(&world, second, "land_repair").await;
    world.harness.set_land_state(
        land_repair.land_entry_id.unwrap(),
        LandStatus::Published {
            landed_sha: Some("b".repeat(40)),
        },
    );
    assert_eq!(world.harness.run_to_end(second).await, Step::Done);

    for (execution, issue) in [(first, &first_issue), (second, &second_issue)] {
        assert_eq!(
            world.harness.status(execution).await,
            ExecutionStatus::Succeeded
        );
        assert_eq!(issue_now(&world, issue).await.status, IssueStatus::Closed);
    }
    assert_eq!(
        node_statuses(
            &world,
            second,
            &["land", "repair", "land_repair", "repair_2"]
        )
        .await,
        vec![
            AttemptStatus::Failed,
            AttemptStatus::Succeeded,
            AttemptStatus::Succeeded,
            AttemptStatus::Skipped
        ]
    );
    assert_eq!(
        world.harness.launches().len(),
        3,
        "two implementations and exactly one repair"
    );
}

/// #3036: a conflict with nothing live ahead (a conflict with rolling itself)
/// repairs at once and spends the first repair attempt; nothing waits.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_conflict_with_nothing_ahead_repairs_at_once_3036() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Conflicts with rolling").await;
    let execution = start_pipeline(&world, &issue, "pipeline-3036-alone").await;
    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);

    let (repair, _, _) = deliver_verified(&world, execution, "repair").await;
    assert!(
        repair.query().contains("Repair attempt 1 of 3."),
        "{}",
        repair.query()
    );
    assert_eq!(
        world.harness.land_conflict_scans(),
        vec![land.land_entry_id.unwrap()]
    );
    assert!(
        topology_events(&world, execution, rows::LAND_CONFLICT_WAIT)
            .await
            .is_empty()
    );
    let land = world.harness.attempt(execution, "land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Failed);
    assert_eq!(
        land.error.as_deref(),
        Some("merge queue entry refused: queue_batch_merge_conflict")
    );
}

/// #3036: the entry a refused land waits for is itself refused. That releases
/// the wait and the repair proceeds, naming how the entry ended.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_refused_entry_ahead_releases_the_wait_and_the_repair_proceeds_3036() {
    let world = World::new(pipeline_manager()).await;
    let first_issue = open_issue(&world, "Refused ahead").await;
    let second_issue = open_issue(&world, "Waits behind").await;
    let first = start_pipeline(&world, &first_issue, "pipeline-3036-refused-ahead").await;
    let second = start_pipeline(&world, &second_issue, "pipeline-3036-released").await;
    deliver_verified(&world, first, "implement").await;
    let ahead = land_waiting_on(&world, first, "land").await;
    deliver_verified(&world, second, "implement").await;
    let behind = land_waiting_on(&world, second, "land").await;
    let ahead_entry = ahead.land_entry_id.unwrap();
    world
        .harness
        .set_land_conflicts(behind.land_entry_id.unwrap(), vec![ahead_entry]);
    refuse(&world, &behind);
    world.harness.executor.advance(second).await.unwrap();
    assert_eq!(
        world.harness.attempt(second, "land", 0, 1).await.status,
        AttemptStatus::Waiting
    );

    world.harness.set_land_state(
        ahead_entry,
        LandStatus::Refused {
            state: "refused".into(),
            reason: "gate_failed".into(),
        },
    );
    let (repair, _, _) = deliver_verified(&world, second, "repair").await;
    assert!(
        repair.query().contains("Repair attempt 1 of 3."),
        "{}",
        repair.query()
    );
    let error = world
        .harness
        .attempt(second, "land", 0, 1)
        .await
        .error
        .unwrap();
    assert!(error.contains(&format!("{ahead_entry} refused")), "{error}");
}

/// #2892 (2): the attempt budget is three repairs. A land refused after the
/// third repair leaves a visible failed execution and the Issue Open; no
/// fourth repair is launched and nothing is retried. Each repair is its own
/// reported node attempt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_spent_repair_budget_fails_visibly_and_leaves_the_issue_open() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Refused every time").await;
    let execution = start_pipeline(&world, &issue, "pipeline-budget").await;

    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    for (repair, land_node) in [
        ("repair", "land_repair"),
        ("repair_2", "land_repair_2"),
        ("repair_3", "land_repair_3"),
    ] {
        deliver_verified(&world, execution, repair).await;
        let refused = land_waiting_on(&world, execution, land_node).await;
        refuse(&world, &refused);
    }
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);

    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Failed
    );
    let failed = world
        .harness
        .attempt(execution, "land_repair_3", 0, 1)
        .await;
    assert_eq!(failed.status, AttemptStatus::Failed);
    assert_eq!(
        failed.failure_class.as_deref(),
        Some(rows::failure::LAND_REFUSED)
    );
    // The run returns the Issue it held: never closed, never landed.
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Open);
    assert_eq!(
        status_events(&world, &issue).await,
        vec![
            format!("topology-start:{execution}"),
            format!("topology-release:{execution}")
        ]
    );
    assert!(
        topology_events(&world, execution, "topology_landed")
            .await
            .is_empty()
    );
    // implement and three repairs, four lands.
    assert_eq!(
        world.harness.launches().len(),
        4,
        "three repairs, never four"
    );
    assert_eq!(world.harness.land_requests().len(), 4);
}

/// F-004: a missing or stale receipt is not an acceptance. The worker is
/// continued at most twice, then the node fails `verification_failed`, nothing
/// is enqueued and the Issue stays Open.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_missing_or_stale_receipt_never_lands() {
    for stale in [false, true] {
        let world = World::new(pipeline_manager()).await;
        let issue = open_issue(&world, "Unverified").await;
        let execution = start_pipeline(&world, &issue, "pipeline-unverified").await;
        world.harness.executor.advance(execution).await.unwrap();
        let implement = world.harness.attempt(execution, "implement", 0, 1).await;
        world
            .harness
            .commit_and_complete(implement.session_id, "implement-work");
        if stale {
            // A green receipt, but for the fork commit, not the handed-off one.
            scoped_test_job(
                &world.harness,
                implement.session_id,
                JobState::Succeeded,
                green_receipt(&world.harness.base),
            )
            .await;
        }
        for _ in 0..2 {
            world.harness.executor.advance(execution).await.unwrap();
            world
                .harness
                .set_status(implement.session_id, SessionStatus::Completed);
        }
        assert_eq!(world.harness.run_to_end(execution).await, Step::Done);

        let failed = world.harness.attempt(execution, "implement", 0, 1).await;
        assert_eq!(failed.status, AttemptStatus::Failed, "stale={stale}");
        assert_eq!(
            failed.failure_class.as_deref(),
            Some(rows::failure::VERIFICATION_FAILED),
            "stale={stale}"
        );
        let reason = if stale { "stale_head:" } else { "no_receipt:" };
        assert!(
            failed.error.as_deref().unwrap().starts_with(reason),
            "{:?}",
            failed.error
        );
        assert_eq!(world.harness.continues().len(), 2, "stale={stale}");
        assert!(world.harness.land_requests().is_empty(), "stale={stale}");
        assert_eq!(
            world.harness.status(execution).await,
            ExecutionStatus::Failed
        );
        assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Open);
        assert_eq!(
            world.harness.launches().len(),
            1,
            "no repair without a land"
        );
    }
}

// ─── #2688: a land the daemon refuses for review routes to its own review ───

const MIGRATION_REASON: &str = "the source changes a schema migration, so source needs the \
     pre_merge review policy but this execution's graph covers ordinary; nothing was enqueued";

/// Drive an Issue-pipeline starter's implement node to a verified commit whose
/// first land the daemon refuses `land_requires_review`, and return the
/// execution, the verified head and the waiting pre-land review.
async fn refused_for_review(
    world: &World,
    starter: &starters::Starter,
    issue: &rsi_common::types::Issue,
    key: &str,
) -> (Uuid, String, rows::AttemptRow) {
    let execution = start_starter(world, starter, issue, key).await;
    let (_, head, _) = deliver_verified(world, execution, "implement").await;
    world
        .harness
        .script_land_requires_review(Some(MIGRATION_REASON));
    let review = waiting_review(world, execution, "pre_review", 0).await;
    world.harness.script_land_requires_review(None);
    (execution, head, review)
}

/// The migration case of the Issue: the implement result is a migration, the
/// land is refused `land_requires_review`, and the same execution reviews the
/// exact head and lands it. It never blocks, never repeats the implementation
/// and never launches the repair of an ordinary refusal.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_land_requiring_review_routes_to_its_own_review_and_lands() {
    use crate::topology::review_policy::ReviewPolicy;
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Adds a migration through the ordinary route").await;
    let (execution, head, review) =
        refused_for_review(&world, pipeline_starter(), &issue, "migration-routed").await;

    // The land is a routed failure, not a block; nothing was enqueued; the
    // execution keeps running and the Issue stays held by it.
    let land = world.harness.attempt(execution, "land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Failed);
    assert_eq!(
        land.failure_class.as_deref(),
        Some(rows::failure::LAND_REQUIRES_REVIEW)
    );
    assert!(land.error.unwrap().contains("schema migration"));
    assert_eq!(land.land_entry_id, None);
    assert!(world.harness.land_requests().is_empty());
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Running
    );
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );
    // The review is of the exact head the land refused; the ordinary repair
    // route does not run for a review refusal.
    let requests = world.harness.review_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, review.review_assignment_id.unwrap());
    assert_eq!(requests[0].1.source_commit, head);
    assert_eq!(requests[0].1.of_node, "implement");
    assert_eq!(
        node_statuses(&world, execution, &["repair", "land_repair"]).await,
        vec![AttemptStatus::Skipped; 2]
    );

    // Approved: the accepted head lands under the pre-merge cover.
    world.harness.script_reviews([ReviewScript::Accept]);
    let land = land_waiting_on(&world, execution, "pre_land").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1.source_commit, head);
    assert_eq!(
        requests[0].1.acceptance,
        LandAcceptance::Review {
            node: "pre_review".into(),
            assignment_id: review.review_assignment_id.unwrap(),
        }
    );
    assert_eq!(requests[0].1.review_coverage, Some(ReviewPolicy::PreMerge));
    let landed_sha = "e".repeat(40);
    publish(&world, &land, &landed_sha);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    let notes = topology_events(&world, execution, "topology_landed").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["detail"]["landed_sha"], landed_sha);
    assert_eq!(notes[0]["detail"]["source_commit"], head);
    assert_eq!(notes[0]["detail"]["node_id"], "pre_land");
    // One implementation: the review route never redoes it.
    assert_eq!(world.harness.launches().len(), 1);
    assert_eq!(world.harness.review_requests().len(), 1);
}

/// Changes requested repair within the loop bound: the fix is verified and
/// reviewed again, and only the approved fix head lands.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_pre_land_review_changes_requested_fixes_then_lands_the_approved_head() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Migration review asks for a fix").await;
    let (execution, first, _) =
        refused_for_review(&world, pipeline_starter(), &issue, "migration-fix").await;

    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["add the down-path test"])]);
    let (fix, second, _) = deliver_verified_at(&world, execution, "pre_fix", 0).await;
    assert!(
        fix.query().contains("1. add the down-path test"),
        "{}",
        fix.query()
    );
    assert_eq!(fix.base_commit, first);
    assert_ne!(first, second);
    assert!(world.harness.land_requests().is_empty());
    let second_review = waiting_review(&world, execution, "pre_review", 1).await;
    assert_eq!(world.harness.review_requests()[1].1.source_commit, second);

    world.harness.script_reviews([ReviewScript::Accept]);
    let land = land_waiting_on(&world, execution, "pre_land").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1.source_commit, second, "the approved head");
    assert_eq!(
        requests[0].1.acceptance,
        LandAcceptance::Review {
            node: "pre_review".into(),
            assignment_id: second_review.review_assignment_id.unwrap(),
        }
    );
    publish(&world, &land, &"d".repeat(40));
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    // implement and the one fix: nothing else launched.
    assert_eq!(world.harness.launches().len(), 2);
}

/// The post-land route carries the same pre-land review, and a source reviewed
/// before it landed is not reviewed a second time after.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_pipeline_land_requiring_review_reviews_before_landing_only() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Migration through the post-land route").await;
    let (execution, head, review) = refused_for_review(
        &world,
        starter_named(ISSUE_PIPELINE_POST_REVIEW),
        &issue,
        "migration-post-review",
    )
    .await;
    assert_eq!(world.harness.review_requests()[0].1.source_commit, head);
    world.harness.script_reviews([ReviewScript::Accept]);
    let land = land_waiting_on(&world, execution, "pre_land").await;
    assert_eq!(
        world.harness.land_requests()[0].1.acceptance,
        LandAcceptance::Review {
            node: "pre_review".into(),
            assignment_id: review.review_assignment_id.unwrap(),
        }
    );
    publish(&world, &land, &"c".repeat(40));
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    assert_eq!(
        node_statuses(
            &world,
            execution,
            &["post_review", "post_review_repair", "followup"]
        )
        .await,
        vec![AttemptStatus::Skipped; 3]
    );
    // Reviewed once, before landing.
    assert_eq!(world.harness.review_requests().len(), 1);
    assert_eq!(world.harness.launches().len(), 1);
}

/// A land refused for review in a graph without the edge keeps blocking (the
/// unrouted contract), and an ordinary refusal still takes the repair route.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_ordinary_land_refusal_still_takes_repair_not_the_review_route() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Ordinary refusal beside the review route").await;
    let execution = start_pipeline(&world, &issue, "refusal-not-review").await;
    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    deliver_verified(&world, execution, "repair").await;
    assert_eq!(
        node_statuses(&world, execution, &["pre_review", "pre_fix", "pre_land"]).await,
        vec![AttemptStatus::Skipped; 3]
    );
    assert!(world.harness.review_requests().is_empty());
}

/// #2699: the first land is refused for an ordinary gate failure, the repair
/// worker's head adds a migration, and the queue then says that head
/// `land_requires_review`: the same execution reviews that exact repair head,
/// lands what the reviewer accepted and settles succeeded, not blocked.
async fn repair_head_requiring_review_reviews_then_lands(
    starter: &starters::Starter,
    key: &str,
    published: &str,
) {
    use crate::topology::review_policy::ReviewPolicy;
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Repair adds a migration").await;
    let execution = start_starter(&world, starter, &issue, key).await;
    let (_, implemented, _) = deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    let (_, repaired, _) = deliver_verified(&world, execution, "repair").await;
    assert_ne!(implemented, repaired);

    // The queue now says the repaired head needs a pre-land review.
    world
        .harness
        .script_land_requires_review(Some(MIGRATION_REASON));
    let review = waiting_review(&world, execution, "repair_review", 0).await;
    world.harness.script_land_requires_review(None);
    let land_repair = world.harness.attempt(execution, "land_repair", 0, 1).await;
    assert_eq!(land_repair.status, AttemptStatus::Failed);
    assert_eq!(
        land_repair.failure_class.as_deref(),
        Some(rows::failure::LAND_REQUIRES_REVIEW)
    );
    assert_eq!(land_repair.land_entry_id, None);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Running
    );
    // Only the repaired head is under review, and the first review route did
    // not run.
    let requests = world.harness.review_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, review.review_assignment_id.unwrap());
    assert_eq!(requests[0].1.source_commit, repaired);
    assert_eq!(requests[0].1.of_node, "repair");
    assert_eq!(
        node_statuses(&world, execution, &["pre_review", "pre_fix", "pre_land"]).await,
        vec![AttemptStatus::Skipped; 3]
    );

    world.harness.script_reviews([ReviewScript::Accept]);
    let land = land_waiting_on(&world, execution, "repair_land").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 2, "the refused land, then the reviewed one");
    assert_eq!(requests[1].1.source_commit, repaired);
    assert_eq!(
        requests[1].1.acceptance,
        LandAcceptance::Review {
            node: "repair_review".into(),
            assignment_id: review.review_assignment_id.unwrap(),
        }
    );
    assert_eq!(requests[1].1.review_coverage, Some(ReviewPolicy::PreMerge));
    publish(&world, &land, published);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    let notes = topology_events(&world, execution, "topology_landed").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["detail"]["landed_sha"], published);
    assert_eq!(notes[0]["detail"]["source_commit"], repaired);
    assert_eq!(notes[0]["detail"]["node_id"], "repair_land");
    // implement and the one repair: the review route never redoes either.
    assert_eq!(world.harness.launches().len(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_pipeline_repair_requiring_review_routes_to_a_review_of_the_repair_head() {
    repair_head_requiring_review_reviews_then_lands(
        pipeline_starter(),
        "repair-migration",
        &"a".repeat(40),
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_pipeline_repair_requiring_review_routes_to_a_review_of_the_repair_head() {
    repair_head_requiring_review_reviews_then_lands(
        starter_named(ISSUE_PIPELINE_POST_REVIEW),
        "repair-migration-post",
        &"b".repeat(40),
    )
    .await;
}

/// The repair review's fix is bounded and re-reviewed: changes requested send
/// the repaired head through `repair_fix`, and only the approved fix lands.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn repair_review_changes_requested_fixes_then_lands_the_approved_head() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Repair review asks for a fix").await;
    let execution = start_pipeline(&world, &issue, "repair-review-fix").await;
    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    let (_, repaired, _) = deliver_verified(&world, execution, "repair").await;
    world
        .harness
        .script_land_requires_review(Some(MIGRATION_REASON));
    waiting_review(&world, execution, "repair_review", 0).await;
    world.harness.script_land_requires_review(None);

    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["add the down-path test"])]);
    let (fix, fixed, _) = deliver_verified_at(&world, execution, "repair_fix", 0).await;
    assert!(fix.query().contains("1. add the down-path test"));
    assert_eq!(fix.base_commit, repaired);
    let second = waiting_review(&world, execution, "repair_review", 1).await;
    assert_eq!(world.harness.review_requests()[1].1.source_commit, fixed);

    world.harness.script_reviews([ReviewScript::Accept]);
    let land = land_waiting_on(&world, execution, "repair_land").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.last().unwrap().1.source_commit, fixed);
    assert_eq!(
        requests.last().unwrap().1.acceptance,
        LandAcceptance::Review {
            node: "repair_review".into(),
            assignment_id: second.review_assignment_id.unwrap(),
        }
    );
    publish(&world, &land, &"9".repeat(40));
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    // implement, repair and the one fix.
    assert_eq!(world.harness.launches().len(), 3);
}

// ─── `issue-pipeline-reviewed` and `issue-pipeline-post-review` (#2565) ────

/// The pinned `definition_digest` of the bundled review routes' revision 1.
/// A change to either text moves its pin on purpose (see the seed test).
const REVIEWED_DIGEST: &str =
    "sha256:0e93e7c928315ce23da0a955e64d23ea552bede7c2e62ac97de949a30ff9c5ce";
const POST_REVIEW_DIGEST: &str =
    "sha256:5d7e93f6fa4b8b8903a6f796de1e111b37c2d925a34f0655eec3e78aa21cc014";

pub(super) fn starter_named(name: &str) -> &'static starters::Starter {
    STARTERS
        .iter()
        .find(|starter| starter.name == name)
        .unwrap_or_else(|| panic!("{name} is bundled"))
}

fn json_node<'a>(value: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == id)
        .unwrap_or_else(|| panic!("node {id}"))
}

fn node_step(definition: &rsi_common::types::TopologyDefinition, id: &str) -> TopologyStep {
    definition
        .nodes
        .iter()
        .find(|node| node.id == id)
        .unwrap_or_else(|| panic!("node {id}"))
        .step()
        .unwrap()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn review_route_starters_have_their_own_digests_and_the_planned_shape() {
    let drifted: Vec<_> = [
        (ISSUE_PIPELINE_REVIEWED, REVIEWED_DIGEST),
        (ISSUE_PIPELINE_POST_REVIEW, POST_REVIEW_DIGEST),
    ]
    .into_iter()
    .filter_map(|(name, pin)| {
        let digest = crate::topology::store::digest(&starter_named(name).canonical_text().unwrap());
        (digest != pin).then(|| format!("{name}: {digest}"))
    })
    .collect();
    assert!(drifted.is_empty(), "pin moved: {drifted:?}");
    assert_eq!(
        STARTERS
            .iter()
            .map(|starter| starter.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        STARTERS.len(),
        "fixed, distinct identities"
    );

    // Reviewed: every land is admitted by a review verdict, never by the
    // session's own receipt, so nothing is enqueued before an accepted review
    // of the exact head, and the repair has a review of its own.
    let reviewed = starter_named(ISSUE_PIPELINE_REVIEWED).definition().unwrap();
    let ids: Vec<_> = reviewed.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "implement",
            "review",
            "fix",
            "land",
            "repair",
            "review_repair",
            "land_repair"
        ]
    );
    for (land, accepted) in [("land", "review"), ("land_repair", "review_repair")] {
        assert!(
            matches!(
                node_step(&reviewed, land),
                TopologyStep::Land { accepted: a, test_filters, close_issue: true }
                    if a == accepted && test_filters.is_empty()
            ),
            "{land}"
        );
    }
    for (review, of, rounds) in [("review", "implement", 2), ("review_repair", "repair", 1)] {
        assert!(
            matches!(
                node_step(&reviewed, review),
                TopologyStep::Review { of: o, reviewer, max_rounds }
                    if o == of
                        && max_rounds == rounds
                        && reviewer.provider == SessionProvider::Codex
                        && reviewer.model == "gpt-6-luna"
                        && reviewer.effort == "high"
            ),
            "{review}"
        );
    }
    // Every commit producer is verified by its own final-HEAD receipt, the fix
    // and the repair included: a changed source never rides on an old receipt.
    for id in ["implement", "fix", "repair"] {
        assert!(
            matches!(
                node_step(&reviewed, id),
                TopologyStep::Session {
                    expects_commit: true,
                    require_verified: true,
                    ..
                }
            ),
            "{id}"
        );
    }
    // The fix loop is the only cycle; no land sits on it.
    let loops: Vec<_> = reviewed
        .edges
        .iter()
        .filter(|edge| edge.loop_edge)
        .map(|edge| (edge.from.as_str(), edge.to.as_str()))
        .collect();
    assert_eq!(loops, [("fix", "review")]);

    // Post-review: the ordinary graph unchanged, plus one review per landing
    // path (the ordinary head and each of the three repairs, #2899) and one
    // follow-up filer with explicit base custody.
    let post = starter_named(ISSUE_PIPELINE_POST_REVIEW)
        .definition()
        .unwrap();
    let ids: Vec<_> = post.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "implement",
            "land",
            "repair",
            "land_repair",
            "repair_2",
            "land_repair_2",
            "repair_3",
            "land_repair_3",
            "post_review",
            "post_review_repair",
            "post_review_repair_2",
            "post_review_repair_3",
            "followup",
            "pre_review",
            "pre_fix",
            "pre_land",
            "repair_review",
            "repair_fix",
            "repair_land"
        ]
    );
    let ordinary = pipeline_starter().definition().unwrap();
    // #2899: the whole #2892 repair chain is the ordinary pipeline's, node for
    // node, so a second rolling advance in the same file is repaired on this
    // route too.
    for id in [
        "implement",
        "land",
        "repair",
        "land_repair",
        "repair_2",
        "land_repair_2",
        "repair_3",
        "land_repair_3",
        "pre_review",
        "pre_fix",
        "pre_land",
        "repair_review",
        "repair_fix",
        "repair_land",
    ] {
        assert_eq!(
            serde_json::to_value(post.nodes.iter().find(|n| n.id == id).unwrap()).unwrap(),
            serde_json::to_value(ordinary.nodes.iter().find(|n| n.id == id).unwrap()).unwrap(),
            "{id} is the ordinary pipeline's"
        );
    }
    for (review, of) in [
        ("post_review", "implement"),
        ("post_review_repair", "repair"),
        ("post_review_repair_2", "repair_2"),
        ("post_review_repair_3", "repair_3"),
    ] {
        assert!(
            matches!(
                node_step(&post, review),
                TopologyStep::Review { of: o, reviewer, max_rounds: 2 }
                    if o == of && reviewer.provider == SessionProvider::Codex
            ),
            "{review}"
        );
    }
    // #2688: the same pre-land route as the ordinary pipeline, and its fix loop
    // is the only cycle.
    assert_pre_land_review_route(&post, "pre_land");
    assert_repair_chain(&post);
    // Each post-land review waits on exactly the land of the repair it reviews.
    let post_edges: Vec<_> = post
        .edges
        .iter()
        .filter(|edge| edge.to.starts_with("post_review") || edge.to == "followup")
        .map(|edge| (edge.from.as_str(), edge.to.as_str()))
        .collect();
    assert_eq!(
        post_edges,
        [
            ("land", "post_review"),
            ("land_repair", "post_review_repair"),
            ("land_repair_2", "post_review_repair_2"),
            ("land_repair_3", "post_review_repair_3"),
            ("post_review", "followup"),
            ("post_review_repair", "followup"),
            ("post_review_repair_2", "followup"),
            ("post_review_repair_3", "followup"),
        ]
    );
    assert!(matches!(
        node_step(&post, "followup"),
        TopologyStep::Session {
            expects_commit: false,
            pass_content: true,
            require_verified: false,
            structured_handoff: false,
        }
    ));
    let text = starter_named(ISSUE_PIPELINE_POST_REVIEW)
        .canonical_text()
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let followup = json_node(&value, "followup");
    assert_eq!(followup["params"]["custody"]["from"], "base");
    assert_eq!(
        followup["params"]["when"],
        serde_json::json!({
            "post_review": "verdict_changes_requested",
            "post_review_repair": "verdict_changes_requested",
            "post_review_repair_2": "verdict_changes_requested",
            "post_review_repair_3": "verdict_changes_requested"
        })
    );
    // The filer dedupes and publishes only into its own project.
    let instructions = followup["params"]["instructions"].as_str().unwrap();
    for needle in [
        "title_contains",
        "idempotency_key post-land-review:",
        "never pass project_id",
        "never reopen the landed Issue",
        "Do not edit files and do not commit",
        "PIPELINE HANDOFF — REVIEW:",
    ] {
        assert!(instructions.contains(needle), "followup lacks {needle:?}");
    }
    let parsed = rsi_common::agent_contract::parse_pipeline_handoff_v2(
        "PIPELINE HANDOFF — REVIEW:\ndoc_path: inline\nmanifest_path: inline\n\
         status: complete\nsummary: none new\n",
        "",
    )
    .expect("the filer's handoff parses");
    assert_eq!(
        parsed.strict_status,
        rsi_common::agent_contract::PipelineStatusV2::Complete
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn review_route_prompts_carry_the_verification_contract_and_the_reviewer_hand_off() {
    let reviewed: serde_json::Value = serde_json::from_str(
        &starter_named(ISSUE_PIPELINE_REVIEWED)
            .canonical_text()
            .unwrap(),
    )
    .unwrap();
    for id in ["implement", "fix", "repair"] {
        let instructions = json_node(&reviewed, id)["params"]["instructions"]
            .as_str()
            .unwrap()
            .to_lowercase();
        for needle in [
            "one job",
            "\"wake\":\"none\"",
            "one agentschedulewake",
            "jobs_terminal",
            "end your turn without a handoff",
            "never commit after your last verification",
            "doc_path: inline, manifest_path: inline, status: complete",
            "actual numeric exit code",
            "independent reviewer",
        ] {
            assert!(
                instructions.contains(needle),
                "{id} prompt lacks {needle:?}"
            );
        }
        assert!(!instructions.contains("lands your work only if"), "{id}");
    }
    // The fix carries the findings as upstream content.
    assert!(matches!(
        node_step(
            &starter_named(ISSUE_PIPELINE_REVIEWED).definition().unwrap(),
            "fix"
        ),
        TopologyStep::Session {
            pass_content: true,
            ..
        }
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn review_route_launch_triples_are_validated_against_the_grant_with_explicit_refusals() {
    for name in [ISSUE_PIPELINE_REVIEWED, ISSUE_PIPELINE_POST_REVIEW] {
        let topology = starter_topology(name);
        let checked = agent::check_definition(&topology, &live_grant(), 0);
        assert!(
            checked.structural.is_empty(),
            "{name}: {:?}",
            checked.structural
        );
        assert!(checked.policy.is_empty(), "{name}: {:?}", checked.policy);

        // An unavailable cross-family reviewer is refused, naming the reviewer.
        let no_codex: Vec<_> = live_grant()
            .into_iter()
            .filter(|launch| launch.provider != SessionProvider::Codex)
            .collect();
        let refused = agent::check_definition(&topology, &no_codex, 0);
        assert!(
            refused
                .policy
                .iter()
                .any(|d| d.contains("reviewer") && d.contains("allowed_launches")),
            "{name}: {:?}",
            refused.policy
        );
        // So is an author-side launch the grant lacks.
        let no_sonnet: Vec<_> = live_grant()
            .into_iter()
            .filter(|launch| launch.model != "claude-sonnet-5-5")
            .collect();
        assert!(
            !agent::check_definition(&topology, &no_sonnet, 0)
                .policy
                .is_empty(),
            "{name}"
        );
        // An empty grant fails closed for an agent.
        assert!(
            !agent::check_definition(&topology, &[], 0).policy.is_empty(),
            "{name}"
        );

        // A reviewer of the author's own vendor family is refused.
        let mut same_family = topology.clone();
        for node in &mut same_family.definition.nodes {
            if let Some(reviewer) = node
                .params
                .get_mut("step")
                .and_then(|s| s.get_mut("reviewer"))
            {
                *reviewer = serde_json::json!({
                    "provider": "claude", "model": "claude-opus-5-5", "effort": "high"
                });
            }
        }
        let refused = agent::check_definition(&same_family, &live_grant(), 0);
        assert!(
            refused
                .policy
                .iter()
                .any(|d| d.contains("different vendor family")),
            "{name}: {:?}",
            refused.policy
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_route_seeds_are_idempotent_and_keep_edited_or_archived_definitions() {
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], live_grant(), 64)).await;
    {
        let store = world.harness.executor.store.lock().await;
        let first = starters::seed_all(&store);
        for name in [ISSUE_PIPELINE_REVIEWED, ISSUE_PIPELINE_POST_REVIEW] {
            assert!(first.contains(&(name, SeedOutcome::Inserted)), "{first:?}");
        }
    }
    for name in [ISSUE_PIPELINE_REVIEWED, ISSUE_PIPELINE_POST_REVIEW] {
        let starter = starter_named(name);
        {
            let store = world.harness.executor.store.lock().await;
            for _ in 0..2 {
                // Earlier names in this loop were edited or archived on
                // purpose; this one is still the untouched seed.
                assert!(starters::seed_all(&store).contains(&(name, SeedOutcome::Unchanged)));
            }
        }
        let (revision, owner_kind, shared, text) = topology_row(&world, name).await;
        assert_eq!((revision, owner_kind.as_str(), shared), (1, "operator", 1));
        assert_eq!(text, starter.canonical_text().unwrap());

        let edited = {
            let store = world.harness.executor.store.lock().await;
            let mut topology = starter_topology(name);
            topology.definition.nodes[0].label = "Operator relabelled".into();
            topology.updated_at = chrono::Utc::now();
            store.update_topology(&topology).unwrap();
            serde_json::to_string(&topology.definition).unwrap()
        };
        for _ in 0..2 {
            let store = world.harness.executor.store.lock().await;
            assert!(starters::seed_all(&store).contains(&(name, SeedOutcome::KeptOperatorEdit)));
        }
        let (revision, _, _, text) = topology_row(&world, name).await;
        assert_eq!((revision, text), (1, edited), "{name}");

        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topologies SET archived_at='2026-10-15T00:00:00.000000000Z' WHERE id=?1",
                [starter.id.to_string()],
            )
            .unwrap();
        assert!(starters::seed_all(&store).contains(&(name, SeedOutcome::KeptArchived)));
    }
}

/// Advance until `node` at `iteration` is `Running` (its session launched).
async fn running_attempt(
    world: &World,
    execution: Uuid,
    node: &str,
    iteration: u32,
) -> rows::AttemptRow {
    for _ in 0..8 {
        world.harness.executor.advance(execution).await.unwrap();
        if let Some(attempt) = world
            .harness
            .attempts(execution)
            .await
            .into_iter()
            .find(|a| {
                a.node_id == node && a.iteration == iteration && a.status == AttemptStatus::Running
            })
        {
            return attempt;
        }
    }
    panic!("{node}@{iteration} never launched");
}

/// [`deliver_verified`] for a node that may sit behind earlier nodes or in a
/// later loop iteration.
pub(super) async fn deliver_verified_at(
    world: &World,
    execution: Uuid,
    node: &str,
    iteration: u32,
) -> (rows::AttemptRow, String, Uuid) {
    let attempt = running_attempt(world, execution, node, iteration).await;
    let head = world
        .harness
        .commit_and_complete(attempt.session_id, &format!("{node}-{iteration}-work"));
    let job = scoped_test_job(
        &world.harness,
        attempt.session_id,
        JobState::Succeeded,
        green_receipt(&head),
    )
    .await;
    (attempt, head, job)
}

/// Advance until the review `node` at `iteration` waits on its verdict.
pub(super) async fn waiting_review(
    world: &World,
    execution: Uuid,
    node: &str,
    iteration: u32,
) -> rows::AttemptRow {
    for _ in 0..8 {
        world.harness.executor.advance(execution).await.unwrap();
        if let Some(review) = world
            .harness
            .attempts(execution)
            .await
            .into_iter()
            .find(|a| {
                a.node_id == node && a.iteration == iteration && a.status == AttemptStatus::Waiting
            })
        {
            return review;
        }
    }
    panic!("review {node}@{iteration} never waited");
}

fn publish(world: &World, land: &rows::AttemptRow, sha: &str) {
    world.harness.set_land_state(
        land.land_entry_id.unwrap(),
        LandStatus::Published {
            landed_sha: Some(sha.to_owned()),
        },
    );
}

/// F-003: the reviewed route enqueues only after an accepted review of the
/// exact head. A fix is re-verified and re-reviewed, and a refused landing's
/// repair is verified and reviewed again: no approval carries over to another
/// source.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn reviewed_route_enqueues_only_after_exact_head_approval_even_for_repaired_source() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Migration needs a pre-merge review").await;
    let execution = start_starter(
        &world,
        starter_named(ISSUE_PIPELINE_REVIEWED),
        &issue,
        "reviewed-repair",
    )
    .await;

    let (_, first, _) = deliver_verified_at(&world, execution, "implement", 0).await;
    let review = waiting_review(&world, execution, "review", 0).await;
    // Verified is not approved: nothing is enqueued while the review waits.
    let requests = world.harness.review_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, review.review_assignment_id.unwrap());
    assert_eq!(requests[0].1.source_commit, first);
    assert_eq!(requests[0].1.of_node, "implement");
    let reviewer = &requests[0].1.reviewer;
    assert_eq!(
        (reviewer.provider, reviewer.model.as_str()),
        (SessionProvider::Codex, "gpt-6-luna")
    );
    assert!(world.harness.land_requests().is_empty());
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );

    // Changes requested: a verified fix, then a second review of the FIX head.
    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["handle the empty case"])]);
    let (fix, second, _) = deliver_verified_at(&world, execution, "fix", 0).await;
    assert!(
        fix.query().contains("1. handle the empty case"),
        "{}",
        fix.query()
    );
    assert_eq!(fix.base_commit, first);
    assert_ne!(first, second);
    assert!(world.harness.land_requests().is_empty());
    let second_review = waiting_review(&world, execution, "review", 1).await;
    assert_eq!(world.harness.review_requests()[1].1.source_commit, second);
    world.harness.script_reviews([ReviewScript::Accept]);
    let land = land_waiting_on(&world, execution, "land").await;

    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].1.source_commit, second,
        "the approved head, not the first"
    );
    assert_eq!(
        requests[0].1.acceptance,
        LandAcceptance::Review {
            node: "review".into(),
            assignment_id: second_review.review_assignment_id.unwrap(),
        }
    );
    assert!(requests[0].1.test_filters.is_empty());

    // The queue refuses: the repaired source is verified and gets its own
    // review before anything is enqueued again.
    refuse(&world, &land);
    let (repair, repaired, _) = deliver_verified_at(&world, execution, "repair", 0).await;
    assert!(repair.query().contains("queue_batch_merge_conflict"));
    let repair_review = waiting_review(&world, execution, "review_repair", 0).await;
    assert_eq!(
        world.harness.land_requests().len(),
        1,
        "no enqueue before approval"
    );
    let requests = world.harness.review_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].1.source_commit, repaired);
    assert_eq!(requests[2].1.of_node, "repair");
    assert_ne!(repaired, second);
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );

    world.harness.script_reviews([ReviewScript::Accept]);
    let land_repair = land_waiting_on(&world, execution, "land_repair").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].1.source_commit, repaired);
    assert_eq!(
        requests[1].1.acceptance,
        LandAcceptance::Review {
            node: "review_repair".into(),
            assignment_id: repair_review.review_assignment_id.unwrap(),
        }
    );
    let landed_sha = "c".repeat(40);
    publish(&world, &land_repair, &landed_sha);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    let notes = topology_events(&world, execution, "topology_landed").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["detail"]["landed_sha"], landed_sha);
    assert_eq!(notes[0]["detail"]["source_commit"], repaired);
    assert_eq!(notes[0]["detail"]["node_id"], "land_repair");
    // implement, fix, repair: no other session ever launched.
    assert_eq!(world.harness.launches().len(), 3);
}

/// F-004: a restart while the pre-merge review waits adopts its assignment; the
/// accepted verdict admits the landing and nothing is requested twice.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn reviewed_route_waiting_review_survives_a_restart() {
    let mut world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Restart during the pre-merge review").await;
    let execution = start_starter(
        &world,
        starter_named(ISSUE_PIPELINE_REVIEWED),
        &issue,
        "reviewed-restart",
    )
    .await;
    let (_, head, _) = deliver_verified_at(&world, execution, "implement", 0).await;
    let waiting = waiting_review(&world, execution, "review", 0).await;

    world.harness.restart();
    let ids = {
        let store = world.harness.executor.store.lock().await;
        rows::drivable_execution_ids(&store, 10).unwrap()
    };
    assert_eq!(ids, vec![execution]);
    let after = waiting_review(&world, execution, "review", 0).await;
    assert_eq!(after.review_assignment_id, waiting.review_assignment_id);
    assert_eq!(
        world.harness.review_requests().len(),
        1,
        "adopted, not re-requested"
    );

    world.harness.script_reviews([ReviewScript::Accept]);
    let land = land_waiting_on(&world, execution, "land").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1.source_commit, head);
    assert_eq!(
        requests[0].1.acceptance,
        LandAcceptance::Review {
            node: "review".into(),
            assignment_id: waiting.review_assignment_id.unwrap(),
        }
    );
    publish(&world, &land, &"d".repeat(40));
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    assert_eq!(world.harness.review_requests().len(), 1);
}

/// Drive the post-review starter to its first waiting post-land review, with
/// the Issue already closed by publication. Returns the execution, the landed
/// commit and the published SHA.
async fn post_review_waiting(
    world: &World,
    issue: &rsi_common::types::Issue,
    key: &str,
) -> (Uuid, String, String) {
    let execution =
        start_starter(world, starter_named(ISSUE_PIPELINE_POST_REVIEW), issue, key).await;
    let (_, head, _) = deliver_verified(world, execution, "implement").await;
    let land = land_waiting_on(world, execution, "land").await;
    let landed_sha = "e".repeat(40);
    publish(world, &land, &landed_sha);
    world.harness.set_land_gate(
        land.land_entry_id.unwrap(),
        crate::topology::land::GateReceipt {
            state: "published".into(),
            test_filters: vec!["rsid=topology::review".into(), "rsi-common=queue".into()],
            log_glob: Some("/queue/logs/batch-*.log".into()),
        },
    );
    waiting_review(world, execution, "post_review", 0).await;
    (execution, head, landed_sha)
}

/// F-003/F-008: publication automatically creates one review assignment for the
/// source that landed, by the other model family, after the Issue is closed and
/// with no manager call; requested changes file a follow-up and finish the
/// execution without touching the closed Issue.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_starter_dispatches_after_close_once() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Custody change gets a post-land review").await;
    let (execution, head, landed_sha) = post_review_waiting(&world, &issue, "post-review").await;

    // Closed first; then exactly one review, of the source that landed.
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    for _ in 0..3 {
        world.harness.executor.advance(execution).await.unwrap();
    }
    let requests = world.harness.review_requests();
    assert_eq!(
        requests.len(),
        1,
        "one assignment, however often the executor ticks"
    );
    let request = &requests[0].1;
    assert_eq!(request.of_node, "implement");
    assert_eq!(request.source_commit, head);
    assert_eq!(request.node_id, "post_review");
    assert_eq!(request.reviewer.provider, SessionProvider::Codex);
    let author = agent::node_launch(
        &crate::session::SessionManager::bridge_topology(&starter_topology(
            ISSUE_PIPELINE_POST_REVIEW,
        ))
        .unwrap()
        .nodes
        .into_iter()
        .find(|n| n.id == "implement")
        .unwrap(),
    )
    .unwrap();
    assert!(
        agent::reviewer_family_independent(
            &author,
            &ManagerLaunchChoiceV2 {
                provider: request.reviewer.provider,
                model: request.reviewer.model.clone(),
                effort: Some(request.reviewer.effort.clone()),
            }
        ),
        "the reviewer is the other model family"
    );
    // The reviewer is told both identities: what was reviewed and what is on
    // rolling.
    assert!(request.query.contains(&head), "{}", request.query);
    assert!(request.query.contains(&landed_sha), "{}", request.query);
    assert!(request.query.contains("already been published"));
    // #2570: the reviewer gets the gate's receipt as its test evidence and is
    // told not to re-run it, and how to report an unavailable check.
    for expected in [
        "Merge-queue gate receipt",
        "rsid=topology::review",
        "/queue/logs/batch-*.log",
        "Do not re-run",
        "at most one exact-name test job",
        "infra_",
    ] {
        assert!(
            request.query.contains(expected),
            "{expected}: {}",
            request.query
        );
    }
    // No manager dispatch: the only sessions are the author's, and the Issue's
    // writes are the executor's own start and land-close.
    assert_eq!(world.harness.launches().len(), 1);
    assert_eq!(
        status_events(&world, &issue).await,
        vec![
            format!("topology-start:{execution}"),
            format!(
                "topology-land:{execution}:{}",
                world.harness.attempt(execution, "land", 0, 1).await.id
            )
        ]
    );
    let after_review_untaken = node_statuses(
        &world,
        execution,
        &["repair", "land_repair", "post_review_repair"],
    )
    .await;
    assert_eq!(after_review_untaken, vec![AttemptStatus::Skipped; 3]);

    // Findings flow to one follow-up filer on explicit base custody.
    world.harness.script_reviews([ReviewScript::Changes(vec![
        "the new check never runs on replay",
        "no test covers the denied path",
    ])]);
    let followup = running_attempt(&world, execution, "followup", 0).await;
    assert!(
        followup
            .query()
            .contains("1. the new check never runs on replay")
    );
    assert!(
        followup
            .query()
            .contains("2. no test covers the denied path")
    );
    assert!(followup.query().contains(&head), "{}", followup.query());
    assert_eq!(followup.base_commit, world.harness.base);
    world.harness.complete_without_commit(followup.session_id);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);

    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        world.harness.review_requests().len(),
        1,
        "never a second review"
    );
    assert_eq!(world.harness.launches().len(), 2, "author and one filer");
    // The landed Issue is neither reopened nor failed by the findings.
    let closed = issue_now(&world, &issue).await;
    assert_eq!(closed.status, IssueStatus::Closed);
    assert_eq!(status_events(&world, &issue).await.len(), 2);
    assert_eq!(
        world
            .harness
            .attempt(execution, "followup", 0, 1)
            .await
            .status,
        AttemptStatus::Succeeded
    );
}

/// #2584: the followup prompt shows the exact required first line for both
/// outcomes, and each example it shows parses under the executor's parser.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn post_review_followup_prompt_shows_the_required_first_line_for_both_outcomes() {
    let text = starter_named(ISSUE_PIPELINE_POST_REVIEW)
        .canonical_text()
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let instructions = json_node(&value, "followup")["params"]["instructions"]
        .as_str()
        .unwrap();
    for needle in [
        "MUST begin with the handoff marker as its very first line",
        "'PIPELINE HANDOFF — REVIEW:'",
        "summary: filed #A, #B",
        "summary: nothing new filed; already filed as #N",
        "never before it",
    ] {
        assert!(instructions.contains(needle), "followup lacks {needle:?}");
    }
    for summary in [
        "filed #2583, #2584",
        "nothing new filed; already filed as #2583",
    ] {
        let reply = format!(
            "PIPELINE HANDOFF — REVIEW:\ndoc_path: inline\nmanifest_path: inline\n\
             status: complete\nsummary: {summary}\n"
        );
        let parsed = rsi_common::agent_contract::parse_pipeline_handoff_v2(&reply, "")
            .unwrap_or_else(|error| panic!("{summary}: {error}"));
        assert_eq!(
            parsed.strict_status,
            rsi_common::agent_contract::PipelineStatusV2::Complete
        );
    }
}

const NOTHING_NEW_PROSE_FIRST: &str =
    "Finding 1 is already filed as #2583, so I filed nothing new.";

const NOTHING_NEW_HANDOFF_FIRST: &str = "PIPELINE HANDOFF — REVIEW:\ndoc_path: inline\nmanifest_path: inline\n\
     status: complete\nsummary: nothing new filed; already filed as #2583\n\n\
     Finding 1 is already filed as #2583, so I filed nothing new.\n";

/// Drive a post-review run to its running `followup` node.
async fn post_review_followup(
    world: &World,
    title: &str,
    key: &str,
) -> (Uuid, rows::AttemptRow, String) {
    let issue = open_issue(world, title).await;
    let (execution, _, landed_sha) = post_review_waiting(world, &issue, key).await;
    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["the reviewer's one finding"])]);
    let followup = running_attempt(world, execution, "followup", 0).await;
    (execution, followup, landed_sha)
}

/// #2584: a followup that correctly files nothing new (the reviewer already
/// filed the finding) and opens with the marker ends the execution `succeeded`
/// with no continuation.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_followup_with_nothing_new_succeeds() {
    let world = World::new(pipeline_manager()).await;
    let (execution, followup, _) =
        post_review_followup(&world, "Nothing new to file", "post-review-nothing-new").await;
    world
        .harness
        .complete_with_content(followup.session_id, NOTHING_NEW_HANDOFF_FIRST);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert!(world.harness.continues().is_empty());
}

/// #2584 (execution 0c4181f8): the followup opens with prose, the parser
/// refuses it, the same session is continued once with the error and repeats a
/// well-formed handoff; the execution ends `succeeded`, not `failed`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_followup_malformed_handoff_is_corrected_on_retry() {
    let world = World::new(pipeline_manager()).await;
    let (execution, followup, _) =
        post_review_followup(&world, "Malformed then fixed", "post-review-retry").await;
    world
        .harness
        .complete_with_content(followup.session_id, NOTHING_NEW_PROSE_FIRST);
    world.harness.executor.advance(execution).await.unwrap();

    let continues = world.harness.continues();
    assert_eq!(continues.len(), 1, "one continuation of the same session");
    assert_eq!(continues[0].0, followup.session_id);
    assert!(
        continues[0].1.contains("PIPELINE HANDOFF — <STAGE>:"),
        "{}",
        continues[0].1
    );
    assert_eq!(
        world
            .harness
            .attempt(execution, "followup", 0, 1)
            .await
            .status,
        AttemptStatus::Running
    );

    world
        .harness
        .complete_with_content(followup.session_id, NOTHING_NEW_HANDOFF_FIRST);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        world
            .harness
            .attempt(execution, "followup", 0, 1)
            .await
            .status,
        AttemptStatus::Succeeded
    );
    assert_eq!(world.harness.continues().len(), 1);
}

/// #2584: a followup that is malformed twice fails its node, and the execution
/// settles with the landed SHA and the review verdict, not a bare `failed`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_followup_malformed_twice_fails_with_the_landed_context() {
    let world = World::new(pipeline_manager()).await;
    let (execution, followup, landed_sha) =
        post_review_followup(&world, "Malformed twice", "post-review-twice").await;
    world
        .harness
        .complete_with_content(followup.session_id, NOTHING_NEW_PROSE_FIRST);
    world.harness.executor.advance(execution).await.unwrap();
    assert_eq!(world.harness.continues().len(), 1);

    world
        .harness
        .complete_with_content(followup.session_id, NOTHING_NEW_PROSE_FIRST);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(world.harness.continues().len(), 1, "never a second retry");
    let failed = world.harness.attempt(execution, "followup", 0, 1).await;
    assert_eq!(failed.status, AttemptStatus::Failed);
    assert_eq!(
        failed.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_INVALID)
    );
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Failed
    );
    let error = world.harness.execution_error(execution).await.unwrap();
    assert!(error.contains("node 'followup' failed"), "{error}");
    assert!(
        error.contains(&format!("land landed {landed_sha}")),
        "{error}"
    );
    assert!(
        error.contains("post_review: changes requested on attempt 1"),
        "{error}"
    );
}

/// F-004: an accepted post-land review finishes the execution with no filer.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_accepted_finishes_without_a_filer() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Accepted post-land review").await;
    let (execution, _, _) = post_review_waiting(&world, &issue, "post-review-ok").await;
    world.harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        node_statuses(&world, execution, &["followup", "post_review_repair"]).await,
        vec![AttemptStatus::Skipped; 2]
    );
    assert_eq!(world.harness.launches().len(), 1);
    assert_eq!(world.harness.review_requests().len(), 1);
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
}

/// F-003: after a refusal and a repair, only the repaired landing is reviewed;
/// the untaken branch never instantiates a second review.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_of_a_repaired_landing_reviews_only_the_repair() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Repaired landing gets one post-land review").await;
    let execution = start_starter(
        &world,
        starter_named(ISSUE_PIPELINE_POST_REVIEW),
        &issue,
        "post-review-repair",
    )
    .await;
    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    let (_, repaired, _) = deliver_verified(&world, execution, "repair").await;
    let land_repair = land_waiting_on(&world, execution, "land_repair").await;
    let landed_sha = "f".repeat(40);
    publish(&world, &land_repair, &landed_sha);
    waiting_review(&world, execution, "post_review_repair", 0).await;

    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    let requests = world.harness.review_requests();
    assert_eq!(requests.len(), 1, "no review of the refused source");
    assert_eq!(requests[0].1.node_id, "post_review_repair");
    assert_eq!(requests[0].1.of_node, "repair");
    assert_eq!(requests[0].1.source_commit, repaired);
    assert!(requests[0].1.query.contains(&landed_sha));
    assert!(
        world
            .harness
            .attempts(execution)
            .await
            .iter()
            .all(|attempt| attempt.node_id != "post_review"
                || attempt.status == AttemptStatus::Skipped)
    );

    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["missing regression test"])]);
    let followup = running_attempt(&world, execution, "followup", 0).await;
    assert!(followup.query().contains("1. missing regression test"));
    world.harness.complete_without_commit(followup.session_id);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(world.harness.review_requests().len(), 1);
    assert_eq!(
        world.harness.launches().len(),
        3,
        "author, repair, one filer"
    );
}

/// #2899: two landings that conflict, through the post-review starter. The
/// repaired head is refused again because rolling advanced in the same file
/// while it waited; the executor repairs it a second time on its own and that
/// head lands. Only that second repair is reviewed after landing, and changes
/// requested still become a follow-up that leaves the closed Issue alone.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_second_conflict_is_repaired_again_lands_and_is_reviewed() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Conflicts twice on the post-review route").await;
    let execution = start_starter(
        &world,
        starter_named(ISSUE_PIPELINE_POST_REVIEW),
        &issue,
        "post-review-conflict-twice",
    )
    .await;

    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    let (_, first_repair, _) = deliver_verified(&world, execution, "repair").await;
    let land_repair = land_waiting_on(&world, execution, "land_repair").await;
    // Rolling advanced in the same file while the repair waited.
    refuse(&world, &land_repair);

    let (repair_2, second_repair, job) = deliver_verified(&world, execution, "repair_2").await;
    assert_ne!(first_repair, second_repair);
    let query = repair_2.query();
    assert!(query.contains("Repair attempt 2 of 3."), "{query}");
    assert!(query.contains("queue_batch_merge_conflict"), "{query}");
    // implement, repair, repair_2: the second repair starts from the refused head.
    let launches = world.harness.launches();
    assert_eq!(launches.len(), 3);
    assert_eq!(launches[2].2, first_repair);
    assert_eq!(
        issue_now(&world, &issue).await.status,
        IssueStatus::InProgress
    );

    let land_repair_2 = land_waiting_on(&world, execution, "land_repair_2").await;
    let requests = world.harness.land_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].1.source_commit, second_repair);
    assert_eq!(
        requests[2].1.acceptance,
        LandAcceptance::Verified {
            node: "repair_2".into(),
            session_id: repair_2.session_id,
            job_id: job,
            head: second_repair.clone(),
        }
    );
    let landed_sha = "9".repeat(40);
    publish(&world, &land_repair_2, &landed_sha);
    waiting_review(&world, execution, "post_review_repair_2", 0).await;

    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    let requests = world.harness.review_requests();
    assert_eq!(requests.len(), 1, "only the landed head is reviewed");
    assert_eq!(requests[0].1.node_id, "post_review_repair_2");
    assert_eq!(requests[0].1.of_node, "repair_2");
    assert_eq!(requests[0].1.source_commit, second_repair);
    assert!(requests[0].1.query.contains(&landed_sha));

    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["no test pins the second merge"])]);
    let followup = running_attempt(&world, execution, "followup", 0).await;
    assert!(
        followup
            .query()
            .contains("1. no test pins the second merge")
    );
    world.harness.complete_without_commit(followup.session_id);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);

    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        node_statuses(
            &world,
            execution,
            &[
                "land",
                "repair",
                "land_repair",
                "repair_2",
                "land_repair_2",
                "repair_3",
                "land_repair_3",
                "post_review",
                "post_review_repair",
                "post_review_repair_2",
                "post_review_repair_3",
                "followup",
            ]
        )
        .await,
        vec![
            AttemptStatus::Failed,
            AttemptStatus::Succeeded,
            AttemptStatus::Failed,
            AttemptStatus::Succeeded,
            AttemptStatus::Succeeded,
            AttemptStatus::Skipped,
            AttemptStatus::Skipped,
            AttemptStatus::Skipped,
            AttemptStatus::Skipped,
            AttemptStatus::Succeeded,
            AttemptStatus::Skipped,
            AttemptStatus::Succeeded,
        ]
    );
    assert_eq!(world.harness.review_requests().len(), 1);
    assert_eq!(
        world.harness.launches().len(),
        4,
        "author, two repairs, one filer"
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
}

/// #2899: a third conflict is repaired a third time and that head is the one
/// reviewed after landing; an accepted review finishes with no filer. A refusal
/// after the third repair fails the run visibly with the Issue Open and no
/// post-land review (the budget matches the ordinary pipeline's).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_third_repair_lands_and_is_reviewed_and_a_fourth_refusal_fails() {
    let world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Conflicts three times on the post-review route").await;
    let execution = start_starter(
        &world,
        starter_named(ISSUE_PIPELINE_POST_REVIEW),
        &issue,
        "post-review-conflict-thrice",
    )
    .await;
    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    for (repair, land_node) in [("repair", "land_repair"), ("repair_2", "land_repair_2")] {
        deliver_verified(&world, execution, repair).await;
        let refused = land_waiting_on(&world, execution, land_node).await;
        refuse(&world, &refused);
    }
    let (repair_3, third_repair, _) = deliver_verified(&world, execution, "repair_3").await;
    assert!(
        repair_3.query().contains("Repair attempt 3 of 3."),
        "{}",
        repair_3.query()
    );
    let land_repair_3 = land_waiting_on(&world, execution, "land_repair_3").await;
    publish(&world, &land_repair_3, &"8".repeat(40));
    waiting_review(&world, execution, "post_review_repair_3", 0).await;
    let requests = world.harness.review_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1.of_node, "repair_3");
    assert_eq!(requests[0].1.source_commit, third_repair);

    world.harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Succeeded
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    assert_eq!(
        node_statuses(&world, execution, &["followup"]).await,
        vec![AttemptStatus::Skipped]
    );
    assert_eq!(
        world.harness.launches().len(),
        4,
        "author and three repairs"
    );

    // The same chain with every landing refused ends failed, Issue Open.
    let issue = open_issue(&world, "Refused after the third repair").await;
    let execution = start_starter(
        &world,
        starter_named(ISSUE_PIPELINE_POST_REVIEW),
        &issue,
        "post-review-budget",
    )
    .await;
    deliver_verified(&world, execution, "implement").await;
    let land = land_waiting_on(&world, execution, "land").await;
    refuse(&world, &land);
    for (repair, land_node) in [
        ("repair", "land_repair"),
        ("repair_2", "land_repair_2"),
        ("repair_3", "land_repair_3"),
    ] {
        deliver_verified(&world, execution, repair).await;
        let refused = land_waiting_on(&world, execution, land_node).await;
        refuse(&world, &refused);
    }
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Failed
    );
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Open);
    assert_eq!(
        node_statuses(
            &world,
            execution,
            &[
                "post_review",
                "post_review_repair",
                "post_review_repair_2",
                "post_review_repair_3",
                "followup"
            ]
        )
        .await,
        vec![AttemptStatus::Skipped; 5]
    );
}

/// F-004: a restart while the post-land review waits adopts its assignment,
/// and a reviewer that never gets capacity ends in a typed blocked outcome
/// that leaves the published landing and its closed Issue alone.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn post_review_survives_a_restart_and_blocks_visibly_when_capacity_runs_out() {
    let mut world = World::new(pipeline_manager()).await;
    let issue = open_issue(&world, "Post-land review recovery").await;
    let (execution, head, _) = post_review_waiting(&world, &issue, "post-review-recovery").await;
    let waiting = world.harness.attempt(execution, "post_review", 0, 1).await;

    world.harness.restart();
    let after = waiting_review(&world, execution, "post_review", 0).await;
    assert_eq!(after.id, waiting.id);
    assert_eq!(after.review_assignment_id, waiting.review_assignment_id);
    assert_eq!(
        world.harness.review_requests().len(),
        1,
        "adopted, not re-requested"
    );

    // The reviewer ends `blocked` (no capacity): one retry after the backoff,
    // then a typed blocked outcome. Nothing landed is undone.
    world.harness.script_reviews([
        ReviewScript::Unsettled("manager_review_blocked"),
        ReviewScript::Unsettled("manager_review_blocked"),
    ]);
    for _ in 0..4 {
        world.harness.executor.advance(execution).await.unwrap();
    }
    let first = world.harness.attempt(execution, "post_review", 0, 1).await;
    assert_eq!(first.status, AttemptStatus::Failed);
    assert_eq!(
        world
            .harness
            .attempt(execution, "post_review", 0, 2)
            .await
            .status,
        AttemptStatus::Reserved,
        "backing off"
    );
    {
        let store = world.harness.executor.store.lock().await;
        let old = (chrono::Utc::now() - chrono::TimeDelta::minutes(10))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "UPDATE topology_node_attempts SET finished_at=?2 WHERE id=?1",
                rusqlite::params![first.id.to_string(), old],
            )
            .unwrap();
    }
    assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
    let blocked = world.harness.attempt(execution, "post_review", 0, 2).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(
        blocked.failure_class.as_deref(),
        Some(rows::failure::REVIEW_UNSETTLED)
    );
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Blocked
    );
    assert_eq!(world.harness.review_requests().len(), 2);
    assert_eq!(world.harness.review_requests()[1].1.source_commit, head);
    assert_eq!(issue_now(&world, &issue).await.status, IssueStatus::Closed);
    assert_eq!(world.harness.launches().len(), 1);
}

/// #2651: every Issue-pipeline starter's implement prompt tells the worker how
/// to hand off an already-satisfied Issue, in a form the executor's parser
/// accepts.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn issue_pipeline_starters_prompt_the_already_satisfied_handoff() {
    for name in [
        ISSUE_PIPELINE,
        ISSUE_PIPELINE_REVIEWED,
        ISSUE_PIPELINE_POST_REVIEW,
    ] {
        let starter = STARTERS
            .iter()
            .find(|starter| starter.name == name)
            .expect("starter");
        let definition: serde_json::Value =
            serde_json::from_str(&starter.canonical_text().expect("text")).expect("json");
        let implement = definition["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|node| node["id"] == "implement")
            .expect("implement node");
        let prompt = implement["params"]["instructions"]
            .as_str()
            .expect("prompt");
        assert!(
            prompt.contains("status: already_satisfied, evidence_commit:"),
            "{name}"
        );
        // The final outcome rule is conditional: the `complete` clause is
        // reached only through "Otherwise", after the already-satisfied branch
        // (no commit) and before the blocked branch, so no unconditional
        // "Finish with ... status: complete" contradicts `already_satisfied`.
        let satisfied = prompt
            .find("status: already_satisfied, evidence_commit:")
            .expect("already_satisfied branch");
        let otherwise = prompt
            .find("Otherwise, when you implemented the change and committed it, finish with a PIPELINE HANDOFF — IMPLEMENTATION: block whose fields are doc_path: inline, manifest_path: inline, status: complete,")
            .unwrap_or_else(|| panic!("{name}: conditional complete clause"));
        let blocked = prompt
            .find("status: blocked with blocker, blocker_class")
            .expect("blocked branch");
        assert!(satisfied < otherwise && otherwise < blocked, "{name}");
        assert_eq!(prompt.matches("status: complete").count(), 1, "{name}");
        assert!(
            !prompt.contains(". Finish with a PIPELINE HANDOFF — IMPLEMENTATION: block whose fields are doc_path: inline, manifest_path: inline, status: complete"),
            "{name}: unconditional complete clause"
        );
        for kept in [
            "an ancestor of your base",
            "refuses a missing or non-ancestor evidence_commit",
            "The evidence commit must also already be on origin/rolling",
            "verify the supplied head with your own scoped_test and hand off complete with the commit equal to that base",
            "the commit SHA (the exact head your scoped_test receipt verified)",
            "the scoped_test job id and its actual numeric exit code",
            "blocker_evidence and blocker_question",
        ] {
            assert!(prompt.contains(kept), "{name}: {kept}");
        }
    }
    let reply = format!(
        "PIPELINE HANDOFF — IMPLEMENTATION:\ndoc_path: inline\nmanifest_path: inline\n\
         status: already_satisfied\nevidence_commit: {}\n",
        "a".repeat(40)
    );
    let parsed = rsi_common::agent_contract::parse_pipeline_handoff_v2(&reply, "").expect("parses");
    assert_eq!(
        parsed.strict_status,
        rsi_common::agent_contract::PipelineStatusV2::AlreadySatisfied
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn starter_implement_briefs_route_broken_tooling_to_the_manager() {
    for name in [
        ISSUE_PIPELINE,
        ISSUE_PIPELINE_REVIEWED,
        ISSUE_PIPELINE_POST_REVIEW,
        ISSUE_IMPLEMENT_REVIEW_LAND,
    ] {
        let topology = starter_topology(name);
        let implement = topology
            .definition
            .nodes
            .iter()
            .find(|node| node.id == "implement")
            .unwrap();
        let prompt = implement.params["instructions"].as_str().unwrap();
        assert!(
            prompt.contains("are technical_impasse, answerable by the manager"),
            "{name}"
        );
        assert!(
            prompt.contains("Use resource for a real spending request only"),
            "{name}"
        );
        assert!(
            prompt.contains("Resource defaults to an operator spend gate; only explicit tooling with no money signal may be answered by the manager, and unknown resource phrasing stays operator-owned"),
            "{name}"
        );
        assert!(
            prompt
                .contains("name the amount and currency in blocker_evidence and blocker_question"),
            "{name}"
        );
    }
}

/// #2266: the `rolling-qa-sweep` triage node was told to finish with
/// `PIPELINE HANDOFF — TRIAGE:`, a stage the handoff parser rejects, so the
/// node could only settle `handoff_invalid`. Every `PIPELINE HANDOFF — <STAGE>`
/// that any seeded starter instructs must be a stage
/// `rsi_common::agent_contract::Stage::from_marker` accepts.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn every_starter_instructed_handoff_stage_parses() {
    use rsi_common::agent_contract::Stage;
    const MARKER: &str = "PIPELINE HANDOFF — ";
    let mut seen = Vec::new();
    for starter in STARTERS.iter() {
        let text = starter.canonical_text().expect("text");
        for (at, _) in text.match_indices(MARKER) {
            let stage: String = text[at + MARKER.len()..]
                .chars()
                .take_while(char::is_ascii_alphabetic)
                .collect();
            assert!(
                Stage::from_marker(&stage).is_some(),
                "{}: instructs `{MARKER}{stage}:` which the handoff parser rejects",
                starter.name
            );
            seen.push((starter.name, stage));
        }
    }
    // The triage node's stage is the one the defect was in: keep it covered.
    assert!(
        seen.iter()
            .any(|(name, stage)| *name == ROLLING_QA_SWEEP && stage == "VERIFY"),
        "{seen:?}"
    );
}
