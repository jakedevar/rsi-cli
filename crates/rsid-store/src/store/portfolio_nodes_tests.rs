//! Store tests for portfolio node identity (#1236, hierarchy S2, M1).

use super::*;
use crate::store::harness_manager_v2::ManagerCallerV1;
use crate::test_support::test_session;
use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, MANAGER_PROJECT_NOT_IN_SCOPE, RevokeGlobalManagerRequestV1,
};
use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
use rsi_common::manager_tree::ManagerTreeKindV1;
use rsi_common::types::{Project, SessionProvider};
use std::path::PathBuf;

pub(super) fn project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
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

pub(super) fn session(store: &Store, project: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/portfolio"));
    row.project_id = project;
    store.insert_session(&row).unwrap();
    id
}

pub(super) fn successor_of(store: &Store, seat: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/portfolio"));
    row.continued_from = Some(seat);
    store.insert_session(&row).unwrap();
    id
}

fn job(owner: Uuid, key: Option<&str>) -> super::super::agent_jobs::NewAgentJob {
    let id = Uuid::new_v4();
    super::super::agent_jobs::NewAgentJob {
        id,
        owner_session_id: owner,
        project_id: None,
        name: None,
        params: serde_json::from_value(serde_json::json!({
            "kind":"build", "command":"check", "workspace":true
        }))
        .unwrap(),
        cwd: "/tmp/portfolio".into(),
        unit_name: format!("rsi-job-{id}"),
        log_path: format!("/tmp/{id}.log"),
        status_path: format!("/tmp/{id}.status"),
        idempotency_key: key.map(str::to_string),
        wake: rsi_common::agent_jobs::JobWake::Owner,
    }
}

pub(super) fn launch() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    }
}

pub(super) fn root(
    label: &str,
    seat: Uuid,
    projects: &[Uuid],
    key: &str,
) -> ConfigurePortfolioNodeRequestV1 {
    ConfigurePortfolioNodeRequestV1 {
        node_id: None,
        parent_node_id: None,
        adopt_node_ids: Vec::new(),
        expected_parent_grant_version: None,
        tier_label: label.into(),
        seat_session_id: seat,
        project_ids: projects.to_vec(),
        allowed_launches: vec![launch()],
        policy: ManagerPolicyV2::default(),
        child_policy: None,
        max_direct_reports: 5,
        expected_node_grant_version: 0,
        expected_authority_epoch: 0,
        idempotency_key: key.into(),
    }
}

pub(super) fn create(
    store: &Store,
    request: &ConfigurePortfolioNodeRequestV1,
) -> Result<PortfolioNodeV1> {
    store.configure_portfolio_node(request, PortfolioGrantor::Operator, "operator:test")
}

pub(super) fn edit_of(node: &PortfolioNodeV1, key: &str) -> ConfigurePortfolioNodeRequestV1 {
    ConfigurePortfolioNodeRequestV1 {
        node_id: Some(node.node_id),
        parent_node_id: node.parent_node_id,
        adopt_node_ids: Vec::new(),
        expected_parent_grant_version: None,
        tier_label: node.tier_label.clone(),
        seat_session_id: node.grant.seat_session_id,
        project_ids: node.grant.project_ids.clone(),
        allowed_launches: node.grant.allowed_launches.clone(),
        policy: node.grant.project_policy.clone(),
        child_policy: node.child_policy.clone(),
        max_direct_reports: node.max_direct_reports,
        expected_node_grant_version: node.grant.grant_version,
        expected_authority_epoch: node.authority_epoch,
        idempotency_key: key.into(),
    }
}

pub(super) fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

pub(super) fn refusal<T: std::fmt::Debug>(result: Result<T>) -> String {
    code(result.unwrap_err())
}

/// Plan §4 I3: two roots over [A, B] and [C] reach only their own coverage,
/// and a third root overlapping A is refused by the store write and by a
/// direct coverage insert.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn two_disjoint_roots_reach_only_their_coverage_and_an_overlap_is_refused() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c, d) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
        project(&store, "D"),
    );
    let (seat_ab, seat_c, seat_third) = (
        session(&store, None),
        session(&store, None),
        session(&store, None),
    );
    let ab = create(&store, &root("global", seat_ab, &[a, b], "ab")).unwrap();
    let only_c = create(&store, &root("global", seat_c, &[c], "c")).unwrap();
    assert_ne!(ab.node_id, only_c.node_id);
    assert_eq!(ab.authority_epoch, ab.grant.grant_version);
    assert_eq!(ab.seat_root_session_id, seat_ab);

    for (seat, project, node) in [(seat_ab, a, &ab), (seat_ab, b, &ab), (seat_c, c, &only_c)] {
        let principal = store
            .global_project_principal(seat, project)
            .unwrap()
            .expect("the seat's own coverage");
        assert_eq!(principal.authority_epoch, node.authority_epoch);
        assert_eq!(principal.seat_root, seat);
        assert!(matches!(
            store.resolve_manager_caller(seat, Some(project)).unwrap(),
            ManagerCallerV1::Global(_)
        ));
    }
    for (seat, project) in [(seat_ab, c), (seat_c, a), (seat_c, b)] {
        assert_eq!(
            refusal(store.global_project_principal(seat, project)),
            MANAGER_PROJECT_NOT_IN_SCOPE
        );
        assert_eq!(
            refusal(store.resolve_manager_caller(seat, Some(project))),
            MANAGER_PROJECT_NOT_IN_SCOPE
        );
    }
    assert_eq!(store.global_seat_grant(seat_c).unwrap().project_ids, [c]);

    // A third root over A is refused and writes nothing.
    assert_eq!(
        refusal(create(
            &store,
            &root("global", seat_third, &[a, d], "overlap")
        )),
        MANAGER_SCOPE_OVERLAP
    );
    assert!(store.portfolio_node_for_seat(seat_third).unwrap().is_none());
    // A live third root over D cannot claim A by a direct coverage insert.
    let third = create(&store, &root("global", seat_third, &[d], "d")).unwrap();
    let direct = store.conn.execute(
        "INSERT INTO manager_portfolio_coverage(project_id,depth,node_id,grant_version) VALUES(?1,0,?2,?3)",
        params![a.to_string(), third.node_id.to_string(), third.grant.grant_version],
    );
    assert!(
        direct.is_err(),
        "sibling coverage is disjoint by construction"
    );
    let chain = store.portfolio_chain_for_project(a).unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].node_id, ab.node_id);
    assert_eq!(chain[0].depth, 0);

    // Revoking a root releases its coverage for another root.
    store
        .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
            node_id: only_c.node_id,
            expected_grant_version: only_c.grant.grant_version,
            expected_authority_epoch: only_c.authority_epoch,
            idempotency_key: "revoke-c".into(),
        })
        .unwrap();
    assert!(store.global_project_principal(seat_c, c).unwrap().is_none());
    assert!(store.portfolio_chain_for_project(c).unwrap().is_empty());
    let mut widened = edit_of(&third, "d-and-c");
    widened.project_ids = vec![d, c];
    let third = create(&store, &widened).unwrap();
    assert_eq!(third.grant.project_ids, [d, c]);
    assert!(third.authority_epoch > ab.authority_epoch);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn configure_is_fenced_replayable_and_refuses_bad_seats() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (seat, other) = (session(&store, None), session(&store, None));
    let node = create(&store, &root("pinnacle", seat, &[a], "p1")).unwrap();
    // An exact replay returns the node; a changed request under the key is refused.
    assert_eq!(
        create(&store, &root("pinnacle", seat, &[a], "p1"))
            .unwrap()
            .node_id,
        node.node_id
    );
    assert_eq!(
        refusal(create(&store, &root("pinnacle", seat, &[b], "p1"))),
        PORTFOLIO_IDEMPOTENCY_CONFLICT
    );
    // CAS on the grant version and the epoch.
    let mut stale = edit_of(&node, "p2");
    stale.expected_node_grant_version += 1;
    assert_eq!(refusal(create(&store, &stale)), MANAGER_NODE_STALE);
    let mut stale = edit_of(&node, "p2");
    stale.expected_authority_epoch += 1;
    assert_eq!(refusal(create(&store, &stale)), MANAGER_NODE_STALE);
    let mut relabel = edit_of(&node, "p2");
    relabel.tier_label = "swarm".into();
    assert_eq!(
        refusal(create(&store, &relabel)),
        PORTFOLIO_TIER_LABEL_IMMUTABLE
    );
    // One active grant per seat: a seat holding a node cannot seat another.
    assert_eq!(
        refusal(create(&store, &root("global", seat, &[b], "g1"))),
        GLOBAL_MANAGER_SEAT_UNAVAILABLE
    );
    // #1237: a nested node narrows its parent, and only the operator
    // creates a root.
    let mut nested = root("global", other, &[b], "nested");
    nested.parent_node_id = Some(node.node_id);
    assert_eq!(
        refusal(create(&store, &nested)),
        rsi_common::portfolio_nodes::MANAGER_SCOPE_WIDENED
    );
    assert_eq!(
        refusal(store.configure_portfolio_node(
            &root("global", other, &[b], "by-node"),
            PortfolioGrantor::Node(node.node_id),
            "node",
        )),
        MANAGER_NODE_ROOT_OPERATOR_ONLY
    );
    // A seat replacement keeps the node id and opens a new epoch.
    let mut replace = edit_of(&node, "p3");
    replace.seat_session_id = other;
    let replaced = create(&store, &replace).unwrap();
    assert_eq!(replaced.node_id, node.node_id);
    assert!(replaced.authority_epoch > node.authority_epoch);
    assert_eq!(replaced.seat_root_session_id, other);
    assert!(store.portfolio_node_for_seat(seat).unwrap().is_none());
    assert!(store.global_project_principal(seat, a).unwrap().is_none());
    assert_eq!(store.list_portfolio_nodes(false).unwrap().len(), 1);
}

/// Succession: a context-cap transfer keeps the node id and epoch, the
/// successor holds the predecessor's ledger principal, and the predecessor's
/// token is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn context_cap_transfer_keeps_node_and_epoch_and_refuses_the_predecessor() {
    let store = Store::open_in_memory().unwrap();
    let (a, c) = (project(&store, "A"), project(&store, "C"));
    let (seat, sibling) = (session(&store, None), session(&store, None));
    let node = create(&store, &root("global", seat, &[a], "g")).unwrap();
    let other = create(&store, &root("global", sibling, &[c], "o")).unwrap();
    let before = store
        .global_project_principal(seat, a)
        .unwrap()
        .expect("seat");
    let successor = successor_of(&store, seat);
    use rsi_common::agent_jobs::{AgentJobResultV1, JobState};
    let live = job(seat, Some("live"));
    let queued = job(seat, Some("queued"));
    let terminal = job(seat, Some("terminal"));
    let foreign = job(sibling, Some("live"));
    for row in [&live, &terminal, &foreign] {
        store.insert_agent_job(row, chrono::Utc::now()).unwrap();
    }
    store
        .insert_agent_job_in_state(&queued, chrono::Utc::now(), JobState::Queued)
        .unwrap();
    store
        .settle_agent_job(
            terminal.id,
            JobState::Succeeded,
            &AgentJobResultV1::default(),
            false,
            chrono::Utc::now(),
        )
        .unwrap();
    // A continuation row alone grants nothing.
    assert!(store.list_agent_jobs(successor, 10).unwrap().is_empty());
    assert!(store.transfer_global_seat(seat, successor).unwrap());
    for id in [live.id, queued.id] {
        assert_eq!(
            store
                .get_agent_job(id)
                .unwrap()
                .unwrap()
                .job
                .owner_session_id,
            successor
        );
    }
    assert_eq!(store.list_agent_jobs(successor, 10).unwrap().len(), 2);
    assert_eq!(
        store.list_agent_jobs(seat, 10).unwrap()[0].job.id,
        terminal.id
    );
    assert_eq!(
        store
            .get_agent_job(foreign.id)
            .unwrap()
            .unwrap()
            .job
            .owner_session_id,
        sibling
    );
    let wake = store
        .settle_agent_job(
            live.id,
            JobState::Succeeded,
            &AgentJobResultV1::default(),
            true,
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .get_scheduled_job(&wake)
            .unwrap()
            .unwrap()
            .wake_session_id,
        Some(successor)
    );
    assert!(
        store
            .settle_agent_job(
                live.id,
                JobState::Succeeded,
                &AgentJobResultV1::default(),
                true,
                chrono::Utc::now()
            )
            .unwrap()
            .is_none()
    );
    let moved = store.get_portfolio_node(node.node_id).unwrap().unwrap();
    assert_eq!(moved.node_id, node.node_id);
    assert_eq!(moved.authority_epoch, node.authority_epoch);
    assert_eq!(moved.seat_root_session_id, seat);
    assert_eq!(moved.grant.seat_session_id, successor);
    assert!(moved.grant.grant_version > node.grant.grant_version);
    assert_eq!(
        store.portfolio_chain_for_project(a).unwrap()[0].grant_version,
        moved.grant.grant_version
    );
    let after = store
        .global_project_principal(successor, a)
        .unwrap()
        .expect("successor");
    assert_eq!(after.authority_epoch, before.authority_epoch);
    assert_eq!(after.seat_root, before.seat_root);
    assert_eq!(
        refusal(store.global_project_principal(seat, a)),
        "manager_node_custody_changed"
    );
    // The sibling node is untouched.
    let untouched = store.get_portfolio_node(other.node_id).unwrap().unwrap();
    assert_eq!(untouched.grant.grant_version, other.grant.grant_version);
    assert!(
        store
            .global_project_principal(sibling, c)
            .unwrap()
            .is_some()
    );
    // The predecessor is no longer anyone's seat: a second transfer is a no-op.
    assert!(!store.transfer_global_seat(seat, successor).unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn manager_job_key_collision_rolls_back_seat_succession() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let node = create(&store, &root("global", seat, &[a], "g")).unwrap();
    let successor = successor_of(&store, seat);
    let live = job(seat, Some("shared"));
    store.insert_agent_job(&live, chrono::Utc::now()).unwrap();
    store
        .insert_agent_job(&job(successor, Some("shared")), chrono::Utc::now())
        .unwrap();
    assert_eq!(
        refusal(store.transfer_global_seat(seat, successor)),
        "manager_job_transfer_key_conflict"
    );
    let kept = store.get_portfolio_node(node.node_id).unwrap().unwrap();
    assert_eq!(kept.grant.grant_id, node.grant.grant_id);
    assert_eq!(kept.grant.seat_session_id, seat);
    assert_eq!(
        store
            .get_agent_job(live.id)
            .unwrap()
            .unwrap()
            .job
            .owner_session_id,
        seat
    );
}

/// Retention (plan §4 I10): no deletes, identity is immutable, revoked is
/// final, and an active grant names a live node.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn portfolio_rows_are_retained_identity_immutable_and_revocation_is_final() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let node = create(&store, &root("global", seat, &[a], "g")).unwrap();
    let id = node.node_id.to_string();
    let grant = node.grant.grant_id.to_string();
    let refused_sql = |sql: &str, args: &[&dyn rusqlite::ToSql]| {
        assert!(store.conn.execute(sql, args).is_err(), "{sql}");
    };
    refused_sql("DELETE FROM manager_portfolio_nodes WHERE id=?1", &[&id]);
    refused_sql(
        "DELETE FROM manager_portfolio_coverage WHERE node_id=?1",
        &[&id],
    );
    refused_sql("DELETE FROM global_manager_grants WHERE id=?1", &[&grant]);
    refused_sql(
        "UPDATE manager_portfolio_nodes SET tier_label='pinnacle' WHERE id=?1",
        &[&id],
    );
    refused_sql(
        "UPDATE manager_portfolio_nodes SET authority_epoch=authority_epoch-1 WHERE id=?1",
        &[&id],
    );
    refused_sql(
        "UPDATE manager_portfolio_coverage SET depth=1 WHERE node_id=?1",
        &[&id],
    );
    refused_sql(
        "UPDATE global_manager_grants SET node_id=NULL WHERE id=?1",
        &[&grant],
    );
    refused_sql(
        "UPDATE global_manager_grants SET max_direct_reports=9 WHERE id=?1",
        &[&grant],
    );
    refused_sql(
        "UPDATE global_manager_grants SET grantor='node:00000000-0000-4000-8000-000000000000' WHERE id=?1",
        &[&grant],
    );
    // An active grant must name a live node.
    let other_seat = session(&store, None);
    refused_sql(
        "INSERT INTO global_manager_grants(id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,idempotency_key,created_at,updated_at)
         SELECT ?1,999,?2,'active',project_ids_json,allowed_launches_json,project_policy_json,operator_origin,'raw',created_at,updated_at FROM global_manager_grants WHERE id=?3",
        &[&Uuid::new_v4().to_string(), &other_seat.to_string(), &grant],
    );
    let revoked = store
        .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
            node_id: node.node_id,
            expected_grant_version: node.grant.grant_version,
            expected_authority_epoch: node.authority_epoch,
            idempotency_key: "r".into(),
        })
        .unwrap();
    assert_eq!(revoked.state, "revoked");
    assert_eq!(revoked.grant.state, "revoked");
    // A replay at the same versions returns the revoked node.
    assert_eq!(
        store
            .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
                node_id: node.node_id,
                expected_grant_version: node.grant.grant_version,
                expected_authority_epoch: node.authority_epoch,
                idempotency_key: "r".into(),
            })
            .unwrap()
            .state,
        "revoked"
    );
    refused_sql(
        "UPDATE manager_portfolio_nodes SET state='active' WHERE id=?1",
        &[&id],
    );
    refused_sql(
        "UPDATE global_manager_grants SET state='active' WHERE id=?1",
        &[&grant],
    );
    // Nothing reactivates a revoked node through the store either.
    assert_eq!(
        refusal(create(&store, &edit_of(&revoked, "again"))),
        MANAGER_NODE_STALE
    );
    let listed = store.list_portfolio_nodes(true).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, "revoked");
    assert!(store.list_portfolio_nodes(false).unwrap().is_empty());
}

fn configure_global(
    store: &Store,
    seat: Uuid,
    projects: &[Uuid],
    expected: i64,
    key: &str,
) -> Result<GlobalManagerGrantV1> {
    store.configure_global_manager(
        &ConfigureGlobalManagerRequestV1 {
            session_id: seat,
            project_ids: projects.to_vec(),
            allowed_launches: vec![launch()],
            project_policy: ManagerPolicyV2::default(),
            expected_grant_version: expected,
            idempotency_key: key.into(),
        },
        "operator:test",
    )
}

/// The `*GlobalManager` shims address the single root labelled `global`;
/// another tier does not disturb them and a second `global` root makes them
/// refuse `global_manager_ambiguous`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_shims_address_the_single_global_root_and_refuse_when_ambiguous() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
    );
    let (seat, pinnacle_seat, second_seat) = (
        session(&store, None),
        session(&store, None),
        session(&store, None),
    );
    let first = configure_global(&store, seat, &[a], 0, "g1").unwrap();
    let node = store.portfolio_node_for_seat(seat).unwrap().unwrap();
    assert_eq!(node.tier_label, GLOBAL_TIER_LABEL);
    assert_eq!(node.grant.grant_id, first.grant_id);
    assert_eq!(
        store.active_global_grant().unwrap().unwrap().grant_id,
        first.grant_id
    );
    // A root of another tier is not the shim's node.
    create(&store, &root("pinnacle", pinnacle_seat, &[b], "p")).unwrap();
    let edited = configure_global(&store, seat, &[a, c], first.grant_version, "g2").unwrap();
    let node_after = store.portfolio_node_for_seat(seat).unwrap().unwrap();
    assert_eq!(node_after.node_id, node.node_id, "the shim keeps the node");
    assert_eq!(node_after.authority_epoch, edited.grant_version);
    assert_eq!(
        refusal(configure_global(
            &store,
            seat,
            &[a],
            first.grant_version,
            "g3"
        )),
        rsi_common::global_manager::GLOBAL_MANAGER_STALE
    );
    // The shim's overlap with the pinnacle root is refused.
    assert_eq!(
        refusal(configure_global(
            &store,
            seat,
            &[a, b],
            edited.grant_version,
            "g4"
        )),
        MANAGER_SCOPE_OVERLAP
    );

    // A second `global` root: every shim refuses, the seats keep working.
    let d = project(&store, "D");
    create(&store, &root("global", second_seat, &[d], "g-second")).unwrap();
    assert_eq!(
        refusal(store.active_global_grant()),
        GLOBAL_MANAGER_AMBIGUOUS
    );
    assert_eq!(
        refusal(store.latest_global_grant()),
        GLOBAL_MANAGER_AMBIGUOUS
    );
    assert_eq!(
        refusal(configure_global(
            &store,
            seat,
            &[a],
            edited.grant_version,
            "g5"
        )),
        GLOBAL_MANAGER_AMBIGUOUS
    );
    assert_eq!(
        refusal(store.revoke_global_manager(&RevokeGlobalManagerRequestV1 {
            expected_grant_version: edited.grant_version,
            idempotency_key: "r".into(),
        })),
        GLOBAL_MANAGER_AMBIGUOUS
    );
    assert!(store.global_seat_grant(seat).is_ok());
    assert!(store.global_seat_grant(second_seat).is_ok());

    // The tree shows each node as a Portfolio row with its label and depth.
    let snapshot = store.manager_tree_snapshot().unwrap();
    let portfolio: Vec<_> = snapshot
        .rows
        .iter()
        .filter(|row| row.kind == ManagerTreeKindV1::Portfolio)
        .collect();
    assert_eq!(portfolio.len(), 3);
    assert!(portfolio.iter().all(|row| row.depth == 0));
    let pinnacle = portfolio
        .iter()
        .find(|row| row.tier_label.as_deref() == Some("pinnacle"))
        .expect("pinnacle row");
    assert_eq!(pinnacle.focus_session_id, Some(pinnacle_seat));
    let b_row = snapshot
        .rows
        .iter()
        .find(|row| row.project_id == Some(b) && row.kind == ManagerTreeKindV1::Project)
        .expect("project B row");
    assert_eq!(b_row.parent_key.as_deref(), Some(pinnacle.key.as_str()));
    assert_eq!(b_row.depth, 1);
    assert_eq!(snapshot.global_grant_version, None, "ambiguous shim");
}

/// The store at V153 (every migration through #1232's V153).
fn store_at_v153() -> Store {
    let store = super::super::raw_in_memory_store_for_test().unwrap();
    for &(step, migrate) in super::super::MIGRATION_STEPS {
        if step >= PORTFOLIO_NODE_SCHEMA_VERSION {
            break;
        }
        migrate(&store, 0).unwrap();
    }
    let version: i32 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, PORTFOLIO_NODE_SCHEMA_VERSION - 1);
    store
}

fn raw_grant(
    store: &Store,
    version: i64,
    seat: Uuid,
    state: &str,
    projects: &[Uuid],
    key: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = stamp();
    store
        .conn
        .execute(
            "INSERT INTO global_manager_grants(id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,idempotency_key,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'operator:test',?8,?9,?9)",
            params![
                id.to_string(),
                version,
                seat.to_string(),
                state,
                serde_json::to_string(projects).unwrap(),
                serde_json::to_string(&vec![launch()]).unwrap(),
                serde_json::to_string(&ManagerPolicyV2::default()).unwrap(),
                key,
                now,
            ],
        )
        .unwrap();
    id
}

/// Backfill: a V153 store with an active v0 grant (a context-cap successor
/// of an operator grant) upgrades to one `global` node with the same seat,
/// projects, policy and S1 epoch; the step is idempotent.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn v153_active_global_grant_backfills_into_one_global_node() {
    let store = store_at_v153();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let deleted_project = Uuid::new_v4();
    let (old_seat, epoch_seat) = (session(&store, None), session(&store, None));
    let successor = successor_of(&store, epoch_seat);
    raw_grant(&store, 1, old_seat, "revoked", &[a], "op-1");
    raw_grant(
        &store,
        2,
        epoch_seat,
        "revoked",
        &[a, b, deleted_project],
        "op-2",
    );
    let active = raw_grant(
        &store,
        3,
        successor,
        "active",
        &[a, b, deleted_project],
        &format!("context-cap:{successor}"),
    );

    store
        .migrate_v154(PORTFOLIO_NODE_SCHEMA_VERSION - 1)
        .unwrap();
    let version: i32 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, PORTFOLIO_NODE_SCHEMA_VERSION);
    for (kind, name) in CATALOG_OBJECTS {
        let present: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2)",
                [kind, name],
                |row| row.get(0),
            )
            .unwrap();
        assert!(present, "{kind} {name}");
    }
    let nodes = store.list_portfolio_nodes(true).unwrap();
    assert_eq!(nodes.len(), 1);
    let node = &nodes[0];
    assert_eq!(node.tier_label, GLOBAL_TIER_LABEL);
    assert_eq!(node.state, "active");
    assert_eq!(node.authority_epoch, 2, "S1's chain head");
    assert_eq!(node.seat_root_session_id, epoch_seat);
    assert_eq!(node.grant.grant_id, active);
    assert_eq!(node.grant.seat_session_id, successor);
    assert_eq!(node.grant.project_ids, [a, b, deleted_project]);
    assert_eq!(node.grant.project_policy, ManagerPolicyV2::default());
    assert_eq!(node.grantor, "operator");
    assert_eq!(node.max_direct_reports, 5);
    // Coverage holds the projects that still exist, at depth 0.
    assert_eq!(store.portfolio_chain_for_project(a).unwrap().len(), 1);
    assert_eq!(store.portfolio_chain_for_project(b).unwrap().len(), 1);
    let unbound: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM global_manager_grants WHERE node_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unbound, 2, "revoked history keeps node_id NULL");
    // The S1 principal is unchanged: same epoch and seat root, and the
    // retired seat of the chain is refused.
    let principal = store
        .global_project_principal(successor, a)
        .unwrap()
        .expect("the active seat");
    assert_eq!(principal.authority_epoch, 2);
    assert_eq!(principal.seat_root, epoch_seat);
    assert_eq!(
        refusal(store.global_project_principal(epoch_seat, a)),
        "manager_node_custody_changed"
    );
    assert!(
        store
            .global_project_principal(old_seat, a)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.active_global_grant().unwrap().unwrap().grant_id,
        active
    );

    // Idempotent: the step is gated on the version and refuses a re-apply.
    store.migrate_v154(PORTFOLIO_NODE_SCHEMA_VERSION).unwrap();
    assert!(apply_migration(&store, PORTFOLIO_NODE_SCHEMA_VERSION).is_err());
    assert_eq!(store.list_portfolio_nodes(true).unwrap().len(), 1);
}

/// A refused upgrade rolls back whole: the V153 schema stays usable.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_refused_v154_upgrade_leaves_the_v153_schema_usable() {
    let store = store_at_v153();
    let a = project(&store, "A");
    let seat = session(&store, None);
    raw_grant(&store, 1, seat, "active", &[a], "op-1");
    // A conflicting object makes the upgrade fail part-way through.
    store
        .conn
        .execute_batch("CREATE TABLE manager_portfolio_coverage(x INTEGER);")
        .unwrap();
    assert!(
        store
            .migrate_v154(PORTFOLIO_NODE_SCHEMA_VERSION - 1)
            .is_err()
    );
    let version: i32 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, PORTFOLIO_NODE_SCHEMA_VERSION - 1);
    let tables: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='manager_portfolio_nodes'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
    let columns: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('global_manager_grants') WHERE name='node_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(columns, 0);
    // V150's one-active index still guards the V153 grants table.
    let second = session(&store, None);
    let duplicate = store.conn.execute(
        "INSERT INTO global_manager_grants(id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,idempotency_key,created_at,updated_at)
         SELECT ?1,2,?2,'active',project_ids_json,allowed_launches_json,project_policy_json,operator_origin,'op-2',created_at,updated_at FROM global_manager_grants",
        params![Uuid::new_v4().to_string(), second.to_string()],
    );
    assert!(duplicate.is_err());
    // Once the conflict is gone the upgrade completes.
    store
        .conn
        .execute_batch("DROP TABLE manager_portfolio_coverage;")
        .unwrap();
    store
        .migrate_v154(PORTFOLIO_NODE_SCHEMA_VERSION - 1)
        .unwrap();
    assert_eq!(store.list_portfolio_nodes(false).unwrap().len(), 1);
}

fn grant_catalog(store: &Store) -> Vec<(String, String, Option<String>)> {
    let mut statement = store
        .conn
        .prepare(
            "SELECT type,name,sql FROM sqlite_master
             WHERE tbl_name LIKE 'global_manager_%' OR tbl_name LIKE 'manager_portfolio_%'
             ORDER BY type,name",
        )
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The rewind fixture undoes M1 exactly: a head store rewound to V153 has
/// the V153 catalog byte for byte, keeps its grant rows, and replays to V154.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_v154_rewind_restores_the_exact_v153_catalog_and_replays() {
    let reference = store_at_v153();
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let node = create(&store, &root("global", seat, &[a], "g")).unwrap();
    // Later migrations (#1239 V156) come off first.
    super::super::tests::rewind_post_v121_tail_to(&store.conn, PORTFOLIO_NODE_SCHEMA_VERSION);
    rewind_to_v153(&store.conn).unwrap();
    assert_eq!(grant_catalog(&store), grant_catalog(&reference));
    let kept: (String, String) = store
        .conn
        .query_row("SELECT id,state FROM global_manager_grants", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(kept, (node.grant.grant_id.to_string(), "active".into()));
    let foreign_keys: i64 = store
        .conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(foreign_keys, 0);
    store
        .migrate_v154(PORTFOLIO_NODE_SCHEMA_VERSION - 1)
        .unwrap();
    let back = store.list_portfolio_nodes(false).unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].grant.grant_id, node.grant.grant_id);
}

pub(super) fn hosted(
    store: &Store,
    project: Uuid,
    parent: Option<Uuid>,
    kind: rsi_common::types::SessionKind,
) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/portfolio"));
    row.project_id = Some(project);
    row.parent_id = parent;
    row.session_kind = kind;
    row.status = rsi_common::types::SessionStatus::Running;
    store.insert_session(&row).unwrap();
    id
}

fn control_root(seat: Uuid, project: Uuid, key: &str) -> ConfigurePortfolioNodeRequestV1 {
    let mut request = root("global", seat, &[project], key);
    request.policy = ManagerPolicyV2 {
        mode: rsi_common::harness_manager_v2::ManagerOperatingModeV2::Execute,
        capabilities: vec![rsi_common::harness_manager_v2::ManagerCapabilityV2::SessionControl],
        ..ManagerPolicyV2::default()
    };
    request
}

/// #1256: a root over A never reaches the seat of a disjoint root B hosted in
/// project A: not to mutate (rule (c) resolves the target's node first), not
/// to read, and not through B's rotation lineage. A's own leaves stay in reach.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_root_never_reaches_a_sibling_roots_seat_hosted_in_its_project() {
    use rsi_common::types::SessionKind;
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let group = hosted(&store, a, None, SessionKind::Group);
    let epic = hosted(&store, a, Some(group), SessionKind::Epic);
    let seat_a = session(&store, None);
    let seat_b = hosted(&store, a, Some(epic), SessionKind::Task);
    let own_leaf = hosted(&store, a, Some(epic), SessionKind::Task);
    create(&store, &control_root(seat_a, a, "root-a")).unwrap();
    let root_b = create(&store, &control_root(seat_b, b, "root-b")).unwrap();

    let principal = store.global_project_principal(seat_a, a).unwrap().unwrap();
    let acting = store
        .global_manager_authority_for(&principal, seat_a)
        .unwrap()
        .config;
    assert!(
        store
            .manager_target_owned_by_ancestor(&acting, seat_b)
            .unwrap()
    );
    assert!(
        !store
            .manager_target_owned_by_ancestor(&acting, own_leaf)
            .unwrap()
    );
    for mutation in [true, false] {
        assert!(
            store
                .manager_session_control_scope(seat_a, seat_b, mutation)
                .unwrap()
                .is_none(),
            "mutation={mutation}"
        );
        assert!(
            store
                .manager_session_control_scope(seat_a, own_leaf, mutation)
                .unwrap()
                .is_some(),
            "own leaf, mutation={mutation}"
        );
    }

    // Through rotation lineage: B's published successor (hosted in A) and,
    // after the transfer, the retired predecessor stay out of A's reach.
    let successor = Uuid::new_v4();
    let mut row = test_session(successor, PathBuf::from("/tmp/portfolio"));
    row.project_id = Some(a);
    row.parent_id = Some(epic);
    row.session_kind = SessionKind::Task;
    row.status = rsi_common::types::SessionStatus::Running;
    row.continued_from = Some(seat_b);
    store.insert_session(&row).unwrap();
    assert_eq!(
        store.portfolio_seat_node(successor).unwrap(),
        Some(root_b.node_id)
    );
    assert!(
        store
            .manager_target_owned_by_ancestor(&acting, successor)
            .unwrap()
    );
    assert!(
        store
            .manager_session_control_scope(seat_a, successor, false)
            .unwrap()
            .is_none()
    );
    assert!(store.transfer_global_seat(seat_b, successor).unwrap());
    for target in [seat_b, successor] {
        assert!(
            store
                .manager_target_owned_by_ancestor(&acting, target)
                .unwrap()
        );
        assert!(
            store
                .manager_session_control_scope(seat_a, target, true)
                .unwrap()
                .is_none()
        );
    }
}

/// #1258: `ConfigureGlobalManager` replays bind to the `global` shim node:
/// another tier's key is a conflict even with an identical request, and a
/// genuine global replay returns its grant.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_configure_replays_bind_to_the_global_shim_node() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (seat, pinnacle_seat) = (session(&store, None), session(&store, None));
    let pinnacle = create(&store, &root("pinnacle", pinnacle_seat, &[b], "shared-key")).unwrap();
    let collision = store.configure_global_manager(
        &ConfigureGlobalManagerRequestV1 {
            session_id: pinnacle_seat,
            project_ids: pinnacle.grant.project_ids.clone(),
            allowed_launches: pinnacle.grant.allowed_launches.clone(),
            project_policy: pinnacle.grant.project_policy.clone(),
            expected_grant_version: 0,
            idempotency_key: "shared-key".into(),
        },
        "operator:test",
    );
    assert_eq!(
        refusal(collision),
        rsi_common::global_manager::GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT
    );
    assert!(store.active_global_grant().unwrap().is_none());

    let first = configure_global(&store, seat, &[a], 0, "g1").unwrap();
    let edited = configure_global(&store, seat, &[a], first.grant_version, "g2").unwrap();
    let replay = configure_global(&store, seat, &[a], 0, "g1").unwrap();
    assert_eq!(replay.grant_id, first.grant_id);
    assert_eq!(
        store.active_global_grant().unwrap().unwrap().grant_id,
        edited.grant_id
    );
    // A context-cap transfer row is never a configure replay.
    let successor = successor_of(&store, seat);
    assert!(store.transfer_global_seat(seat, successor).unwrap());
    assert_eq!(
        refusal(configure_global(
            &store,
            successor,
            &[a],
            0,
            &format!("context-cap:{successor}")
        )),
        rsi_common::global_manager::GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT
    );
}
