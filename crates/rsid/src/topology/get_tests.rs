//! #2414 acceptance tests: `AgentTopologyGet` reads a visible topology's
//! stored definition under the list's scope rules, and a successor manager
//! seat revises a topology its predecessor created. A child module of
//! `agent_tests`, so it shares its `World`.

use super::*;
use rsi_common::topology_agent::{AgentTopologyGetRequestV1, AgentTopologyGetResultV1};

fn by_name(name: &str) -> AgentTopologyGetRequestV1 {
    AgentTopologyGetRequestV1 {
        name: Some(name.into()),
        ..AgentTopologyGetRequestV1::default()
    }
}

fn by_id(id: Uuid) -> AgentTopologyGetRequestV1 {
    AgentTopologyGetRequestV1 {
        topology_id: Some(id),
        ..AgentTopologyGetRequestV1::default()
    }
}

async fn read(
    world: &World,
    caller: Uuid,
    request: &AgentTopologyGetRequestV1,
) -> Result<AgentTopologyGetResultV1> {
    let store = world.harness.executor.store.lock().await;
    agent::get(&store, caller, request)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_manager_reads_a_visible_definition_and_forks_it() {
    let world = World::new(automation()).await;
    let epic_def = definition(&[("A", &luna())], &[]);
    let epic = world
        .upsert(world.lead_a, &upsert_request("get-epic", epic_def.clone()))
        .await
        .unwrap();
    let mgr_def = definition(&[("A", &luna()), ("B", &glm())], &[("A", "B")]);
    let mgr = world
        .upsert(world.manager, &manager_upsert("get-mgr", mgr_def.clone()))
        .await
        .unwrap();

    // By id: the Epic-A topology is in the manager's scope.
    let got = read(&world, world.manager, &by_id(epic.topology_id.unwrap()))
        .await
        .unwrap();
    assert_eq!(got.topology.name, "get-epic");
    assert_eq!(got.topology.owner_kind, "epic");
    assert_eq!(got.topology.epic_id, Some(world.epic_a));
    assert_eq!(got.topology.revision, 1);
    assert_eq!(got.topology.definition_digest, epic.definition_digest);
    assert!(!got.topology.shared);
    assert_eq!(got.owner_session_id, Some(world.lead_a));
    assert_eq!(got.project_id, Some(world.project));
    assert_eq!(got.definition, serde_json::to_value(&epic_def).unwrap());

    // By name: the manager-scope topology, whose digest fences an execute.
    let got = read(&world, world.manager, &by_name("get-mgr"))
        .await
        .unwrap();
    assert_eq!(got.topology.owner_kind, "manager");
    assert_eq!(got.owner_session_id, Some(world.manager));
    assert_eq!(got.topology.definition_digest, mgr.definition_digest);
    let stored: TopologyDefinition = serde_json::from_value(got.definition.clone()).unwrap();
    assert_eq!(stored, mgr_def);

    // Fork: the returned definition goes to a new name unchanged.
    let fork = world
        .upsert(world.manager, &manager_upsert("get-mgr-fork", stored))
        .await
        .unwrap();
    assert_eq!(fork.revision, Some(1));
    assert_ne!(fork.topology_id, mgr.topology_id);
    let forked = read(&world, world.manager, &by_name("get-mgr-fork"))
        .await
        .unwrap();
    assert_eq!(forked.definition, got.definition);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn get_refuses_what_the_caller_cannot_see_without_revealing_it() {
    let world = World::new(automation()).await;
    let epic = world
        .upsert(
            world.lead_a,
            &upsert_request("hidden-epic", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    world
        .upsert(
            world.manager,
            &manager_upsert("hidden-mgr", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let missing = error_code(
        &read(&world, world.manager, &by_name("absent"))
            .await
            .unwrap_err(),
    );
    assert_eq!(missing, "not_found_in_scope");

    // Another Epic's lead sees neither the Epic-A topology nor the manager's
    // unshared one; the refusal is the same as for a name that does not exist.
    for request in [by_id(epic.topology_id.unwrap()), by_name("hidden-mgr")] {
        assert_eq!(
            error_code(&read(&world, world.lead_b, &request).await.unwrap_err()),
            missing
        );
    }
    assert_eq!(
        error_code(
            &read(&world, world.lead_a, &by_name("hidden-mgr"))
                .await
                .unwrap_err()
        ),
        missing
    );
    // A session with no topology authority is refused outright.
    assert_eq!(
        error_code(
            &read(&world, world.worker, &by_name("hidden-epic"))
                .await
                .unwrap_err()
        ),
        "authority_denied"
    );
    // The manager without Automation is refused like the other verbs.
    world
        .grant(policy(
            &[ManagerCapabilityV2::SessionControl],
            vec![luna()],
            64,
        ))
        .await;
    assert_eq!(
        error_code(
            &read(&world, world.manager, &by_name("hidden-mgr"))
                .await
                .unwrap_err()
        ),
        "capability_denied"
    );
    // The selector must name exactly one topology.
    for request in [
        AgentTopologyGetRequestV1::default(),
        AgentTopologyGetRequestV1 {
            topology_id: Some(Uuid::new_v4()),
            name: Some("hidden-epic".into()),
            ..AgentTopologyGetRequestV1::default()
        },
    ] {
        assert_eq!(
            error_code(&read(&world, world.lead_a, &request).await.unwrap_err()),
            "invalid_params"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_successor_seat_revises_its_predecessors_manager_topology() {
    let world = World::new(automation()).await;
    let first = world
        .upsert(
            world.manager,
            &manager_upsert("seat-owned", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();

    // The seat rotates: the predecessor is archived and its published
    // successor is the lineage tip the manager config now resolves to.
    let successor = Uuid::new_v4();
    {
        let store = world.harness.executor.store.lock().await;
        let predecessor = store.get_session(world.manager).unwrap().unwrap();
        store
            .update_session_status(world.manager, SessionStatus::Archived)
            .unwrap();
        let mut next = predecessor;
        next.id = successor;
        next.continued_from = Some(world.manager);
        next.rotation_depth += 1;
        next.status = SessionStatus::Running;
        store.insert_session(&next).unwrap();
        assert!(
            store
                .record_harness_manager_rotation(world.manager, successor)
                .unwrap()
        );
    }
    assert_eq!(
        error_code(
            &world
                .upsert(
                    world.manager,
                    &manager_upsert("seat-owned", definition(&[("A", &glm())], &[])),
                )
                .await
                .unwrap_err()
        ),
        "authority_denied",
        "the retired session itself holds no authority"
    );

    // The successor reads the definition and revises it under the CAS fence.
    let seen = read(&world, successor, &by_name("seat-owned"))
        .await
        .unwrap();
    assert_eq!(seen.topology.revision, 1);
    assert_eq!(seen.owner_session_id, Some(world.manager));
    let mut revise = manager_upsert("seat-owned", definition(&[("A", &glm())], &[]));
    revise.idempotency_key = "successor-revise".into();
    revise.expected_revision = Some(seen.topology.revision);
    let revised = world.upsert(successor, &revise).await.unwrap();
    assert_eq!(revised.topology_id, first.topology_id);
    assert_eq!(revised.revision, Some(2));
    assert!(!revised.deduplicated);
    assert_ne!(revised.definition_digest, first.definition_digest);

    let after = read(&world, successor, &by_id(first.topology_id.unwrap()))
        .await
        .unwrap();
    assert_eq!(after.topology.revision, 2);
    assert_eq!(after.topology.definition_digest, revised.definition_digest);
    assert_eq!(after.owner_session_id, Some(successor));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn names_resolve_within_ownership_and_shared_fallbacks_keep_their_ids() {
    let world = World::new(automation()).await;
    let name = "scope-local-name";
    let mgr = world
        .upsert(
            world.manager,
            &manager_upsert(name, definition(&[("M", &luna())], &[])),
        )
        .await
        .unwrap();
    let a = world
        .upsert(
            world.lead_a,
            &upsert_request(name, definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let b = world
        .upsert(
            world.lead_b,
            &upsert_request(name, definition(&[("B", &luna())], &[])),
        )
        .await
        .unwrap();
    for (caller, row) in [
        (world.manager, &mgr),
        (world.lead_a, &a),
        (world.lead_b, &b),
    ] {
        assert_eq!(
            read(&world, caller, &by_name(name))
                .await
                .unwrap()
                .topology
                .topology_id,
            row.topology_id.unwrap()
        );
        let store = world.harness.executor.store.lock().await;
        let listed = agent::list(&store, caller, &Default::default()).unwrap();
        assert_eq!(listed.topologies.len(), 1);
        assert_eq!(listed.topologies[0].topology_id, row.topology_id.unwrap());
    }
    // A scoped upsert revises only its namespace, including when a different
    // visible row with that name was created first.
    let mut revise = upsert_request(name, definition(&[("A2", &luna())], &[]));
    revise.idempotency_key = "revise-own-name".into();
    revise.expected_revision = Some(1);
    let changed = world.upsert(world.lead_a, &revise).await.unwrap();
    assert_eq!(changed.topology_id, a.topology_id);
    assert_eq!(changed.revision, Some(2));
    assert_eq!(
        read(&world, world.manager, &by_name(name))
            .await
            .unwrap()
            .topology
            .revision,
        1
    );
    assert_eq!(
        read(&world, world.lead_b, &by_name(name))
            .await
            .unwrap()
            .topology
            .revision,
        1
    );
    // Archived names remain reserved in their own namespace; another scope
    // can retain its independent positive resolution.
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topologies SET archived_at=?2 WHERE id=?1",
                rusqlite::params![a.topology_id.unwrap().to_string(), rows::now_text()],
            )
            .unwrap();
        agent::set_shared(&store, mgr.topology_id.unwrap(), true).unwrap();
    }
    assert_eq!(
        read(&world, world.lead_a, &by_name(name))
            .await
            .unwrap()
            .topology
            .topology_id,
        mgr.topology_id.unwrap()
    );
    revise.idempotency_key = "archived-own-name".into();
    assert_eq!(
        error_code(&world.upsert(world.lead_a, &revise).await.unwrap_err()),
        "name_conflict"
    );
}
