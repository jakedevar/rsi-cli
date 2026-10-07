//! Store tests for operator-created levels above global (#1237, hierarchy
//! S3): nesting, adoption, grantor-scoped revoke, unified narrowing and the
//! N-level resolver.

use super::tests::{create, edit_of, hosted, project, refusal, root, session};
use super::*;
use crate::store::global_manager::{GlobalMessage, GlobalMessageDirection};
use crate::store::harness_manager_v2::ManagerCallerV1;
use rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE;
use rsi_common::harness_manager_v2::{
    ManagerActionStateV2, ManagerActionV2, ManagerCapabilityV2, ManagerOperatingModeV2,
};
use rsi_common::portfolio_nodes::{
    MANAGER_CAPABILITY_WIDENED, MANAGER_SCOPE_WIDENED, PORTFOLIO_ADOPT_NOT_ROOT,
    PORTFOLIO_COVERAGE_NOT_SUPERSET, PORTFOLIO_IDEMPOTENCY_CONFLICT, PORTFOLIO_PARENT_IMMUTABLE,
};
use rsi_common::types::SessionKind;

/// The same PM verb set at every level; each finite allowance a little under
/// half its parent's per level, so two siblings fit together (#1302: the
/// children of a parent sum below it). The narrowing rule keeps capabilities
/// equal.
fn tier_policy(level: u16) -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![
            ManagerCapabilityV2::IssueCoordinate,
            ManagerCapabilityV2::SessionCreate,
            ManagerCapabilityV2::SessionControl,
        ],
        max_created_containers: (64 >> level) - 1,
        max_created_sessions: (128 >> level) - 1,
        max_active_sessions: (64 >> level) - 1,
        ..ManagerPolicyV2::default()
    }
}

fn level_request(
    label: &str,
    seat: Uuid,
    projects: &[Uuid],
    parent: Option<&PortfolioNodeV1>,
    level: u16,
    key: &str,
) -> ConfigurePortfolioNodeRequestV1 {
    let mut request = root(label, seat, projects, key);
    request.parent_node_id = parent.map(|parent| parent.node_id);
    request.policy = tier_policy(level);
    request
}

fn node(store: &Store, id: Uuid) -> PortfolioNodeV1 {
    store.get_portfolio_node(id).unwrap().unwrap()
}

/// Every grant and coverage row, to prove a refusal wrote nothing.
fn snapshot(store: &Store) -> (Vec<(String, String)>, Vec<(String, i64, String)>) {
    let mut grants = store
        .conn
        .prepare("SELECT id,state FROM global_manager_grants ORDER BY grant_version")
        .unwrap();
    let grants = grants
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut coverage = store
        .conn
        .prepare(
            "SELECT project_id,depth,node_id FROM manager_portfolio_coverage ORDER BY project_id,depth",
        )
        .unwrap();
    let coverage = coverage
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    (grants, coverage)
}

/// The chain covering `project` as `(depth, node)`.
fn chain(store: &Store, project: Uuid) -> Vec<(u16, Uuid)> {
    store
        .portfolio_chain_for_project(project)
        .unwrap()
        .into_iter()
        .map(|row| (row.depth, row.node_id))
        .collect()
}

/// Build swarm → pinnacle → global over shrinking project sets with the given
/// labels and record what each seat's authority resolves to, by role (never
/// by label).
fn authority_by_role(labels: [&str; 3]) -> Vec<String> {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
    );
    let seats = [
        session(&store, None),
        session(&store, None),
        session(&store, None),
    ];
    let top = create(
        &store,
        &level_request(labels[0], seats[0], &[a, b, c], None, 0, "top"),
    )
    .unwrap();
    let middle = create(
        &store,
        &level_request(labels[1], seats[1], &[a, b], Some(&top), 1, "middle"),
    )
    .unwrap();
    let bottom = create(
        &store,
        &level_request(labels[2], seats[2], &[a], Some(&middle), 2, "bottom"),
    )
    .unwrap();
    let nodes = [top.node_id, middle.node_id, bottom.node_id];
    let mut results = Vec::new();
    for (role, seat) in seats.iter().enumerate() {
        for (name, project) in [("a", a), ("b", b), ("c", c)] {
            let outcome = match store.resolve_manager_caller(*seat, Some(project)) {
                Ok(ManagerCallerV1::Global(authority)) => format!(
                    "pm-set epoch={} root={}",
                    authority.config.row_version == node(&store, nodes[role]).authority_epoch,
                    authority.config.manager_session_id == *seat
                ),
                Ok(other) => format!("other {other:?}"),
                Err(error) => super::tests::code(error),
            };
            results.push(format!("role{role}:{name}:{outcome}"));
        }
    }
    let depths: Vec<String> = chain(&store, a)
        .into_iter()
        .map(|(depth, id)| {
            let role = nodes.iter().position(|node| *node == id).unwrap();
            format!("depth{depth}=role{role}")
        })
        .collect();
    results.extend(depths);
    results
}

/// I11 (store side): levels are data. Three nested nodes cover by depth, each
/// seat holds the PM principal in exactly its own coverage, and permuting the
/// tier labels changes no authority result.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn nested_levels_resolve_by_coverage_and_never_by_tier_label() {
    let results = authority_by_role(["swarm", "pinnacle", "global"]);
    let ok = "pm-set epoch=true root=true";
    for expected in [
        format!("role0:a:{ok}"),
        format!("role0:b:{ok}"),
        format!("role0:c:{ok}"),
        format!("role1:a:{ok}"),
        format!("role1:b:{ok}"),
        format!("role1:c:{MANAGER_PROJECT_NOT_IN_SCOPE}"),
        format!("role2:a:{ok}"),
        format!("role2:b:{MANAGER_PROJECT_NOT_IN_SCOPE}"),
        format!("role2:c:{MANAGER_PROJECT_NOT_IN_SCOPE}"),
        "depth0=role0".to_string(),
        "depth1=role1".to_string(),
        "depth2=role2".to_string(),
    ] {
        assert!(results.contains(&expected), "{expected} in {results:?}");
    }
    assert_eq!(results, authority_by_role(["global", "swarm", "pinnacle"]));
    assert_eq!(results, authority_by_role(["global", "global", "global"]));
    // No authority source reads the display label.
    for (file, source) in [
        (
            "global_manager_authority.rs",
            include_str!("global_manager_authority.rs"),
        ),
        ("manager_actions.rs", include_str!("manager_actions.rs")),
        ("agent_authority.rs", include_str!("agent_authority.rs")),
        ("harness_manager.rs", include_str!("harness_manager.rs")),
    ] {
        assert_eq!(source.matches("tier_label").count(), 0, "{file}");
    }
}

/// Adoption: a new pinnacle adopts two roots. Their subtrees move one level
/// down under new grant versions with their grantor, seat, epoch and ledger
/// principal intact, and each still resolves only inside its own coverage.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn adopting_roots_moves_their_subtrees_down_and_keeps_their_principals() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
    );
    let (seat_g1, seat_g2, seat_r, seat_p) = (
        session(&store, None),
        session(&store, None),
        session(&store, None),
        session(&store, None),
    );
    let g1 = create(
        &store,
        &level_request("global", seat_g1, &[a], None, 1, "g1"),
    )
    .unwrap();
    let g2 = create(
        &store,
        &level_request("global", seat_g2, &[b], None, 1, "g2"),
    )
    .unwrap();
    let region = create(
        &store,
        &level_request("region", seat_r, &[a], Some(&g1), 2, "r"),
    )
    .unwrap();
    let before_g1 = store.global_project_principal(seat_g1, a).unwrap().unwrap();
    let mut appoint = level_request("pinnacle", seat_p, &[a, b, c], None, 0, "p");
    appoint.adopt_node_ids = vec![g1.node_id, g2.node_id];
    let pinnacle = create(&store, &appoint).unwrap();
    assert_eq!(pinnacle.parent_node_id, None);

    for (old, project) in [(&g1, a), (&g2, b)] {
        let moved = node(&store, old.node_id);
        assert_eq!(moved.parent_node_id, Some(pinnacle.node_id));
        assert_eq!(moved.grantor, "operator");
        assert_eq!(moved.authority_epoch, old.authority_epoch);
        assert_eq!(moved.seat_root_session_id, old.seat_root_session_id);
        assert_eq!(moved.grant.seat_session_id, old.grant.seat_session_id);
        assert!(moved.grant.grant_version > old.grant.grant_version);
        assert_eq!(chain(&store, project)[1], (1, old.node_id));
    }
    let moved_region = node(&store, region.node_id);
    assert_eq!(moved_region.parent_node_id, Some(g1.node_id));
    assert_eq!(moved_region.authority_epoch, region.authority_epoch);
    assert_eq!(
        chain(&store, a),
        [(0, pinnacle.node_id), (1, g1.node_id), (2, region.node_id)]
    );
    assert_eq!(chain(&store, c), [(0, pinnacle.node_id)]);
    // The ledger principal is unchanged; only the fence's policy version moved.
    let after_g1 = store.global_project_principal(seat_g1, a).unwrap().unwrap();
    assert_eq!(after_g1.authority_epoch, before_g1.authority_epoch);
    assert_eq!(after_g1.seat_root, before_g1.seat_root);
    // The pinnacle reaches every project of both globals; each global keeps
    // only its own.
    for project in [a, b, c] {
        assert!(
            store
                .global_project_principal(seat_p, project)
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(
        refusal(store.global_project_principal(seat_g1, b)),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
    assert_eq!(
        refusal(store.global_project_principal(seat_g2, a)),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
    // The ancestors of a project principal in A are the whole chain.
    let mut config = store
        .global_manager_authority_for(&after_g1, seat_g1)
        .unwrap()
        .config;
    let (ancestors, own) = store.portfolio_ancestors_of(&config).unwrap();
    assert_eq!(own.unwrap().node_id, g1.node_id);
    assert_eq!(
        ancestors
            .iter()
            .map(|head| head.node_id)
            .collect::<Vec<_>>(),
        [pinnacle.node_id]
    );
    config.manager_session_id = Uuid::new_v4();
    let (ancestors, own) = store.portfolio_ancestors_of(&config).unwrap();
    assert!(own.is_none());
    assert_eq!(ancestors.len(), 3);
    // A replay of the adoption returns the pinnacle and moves nothing again.
    let versions = snapshot(&store);
    assert_eq!(create(&store, &appoint).unwrap().node_id, pinnacle.node_id);
    assert_eq!(snapshot(&store), versions);
}

/// Adopting an overlapping or non-root node, a partial coverage, a wider
/// child or an over-full parent is refused with nothing written; agents
/// never adopt; an edit never re-parents.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn bad_adoptions_are_refused_with_no_partial_write() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
    );
    let (seat_g1, seat_g2, seat_r, seat_p, seat_x) = (
        session(&store, None),
        session(&store, None),
        session(&store, None),
        session(&store, None),
        session(&store, None),
    );
    let g1 = create(
        &store,
        &level_request("global", seat_g1, &[a], None, 1, "g1"),
    )
    .unwrap();
    let g2 = create(
        &store,
        &level_request("global", seat_g2, &[b], None, 1, "g2"),
    )
    .unwrap();
    let region = create(
        &store,
        &level_request("region", seat_r, &[a], Some(&g1), 2, "r"),
    )
    .unwrap();
    let before = snapshot(&store);
    let adopting = |projects: &[Uuid], adopt: Vec<Uuid>, key: &str| {
        let mut request = level_request("pinnacle", seat_p, projects, None, 0, key);
        request.adopt_node_ids = adopt;
        request
    };
    // Overlap: B is held at depth 0 by g2, which is not adopted.
    assert_eq!(
        refusal(create(
            &store,
            &adopting(&[a, b], vec![g1.node_id], "overlap")
        )),
        MANAGER_SCOPE_OVERLAP
    );
    // Not a root: the region sits under g1.
    assert_eq!(
        refusal(create(
            &store,
            &adopting(&[a], vec![region.node_id], "non-root")
        )),
        PORTFOLIO_ADOPT_NOT_ROOT
    );
    // The adopting node must cover every adopted project.
    assert_eq!(
        refusal(create(
            &store,
            &adopting(&[a, c], vec![g1.node_id, g2.node_id], "partial")
        )),
        PORTFOLIO_COVERAGE_NOT_SUPERSET
    );
    // An adopted child must narrow the new node.
    let mut narrow = adopting(&[a, b], vec![g1.node_id, g2.node_id], "narrow");
    narrow.policy.capabilities.pop();
    assert_eq!(refusal(create(&store, &narrow)), MANAGER_CAPABILITY_WIDENED);
    let mut equal = adopting(&[a, b], vec![g1.node_id, g2.node_id], "equal");
    equal.policy = tier_policy(1);
    assert_eq!(refusal(create(&store, &equal)), MANAGER_ALLOWANCE_EXCEEDED);
    let mut crowded = adopting(&[a, b], vec![g1.node_id, g2.node_id], "crowded");
    crowded.max_direct_reports = 1;
    assert_eq!(
        refusal(create(&store, &crowded)),
        MANAGER_ALLOWANCE_EXCEEDED
    );
    // Only the operator adopts.
    let mut by_node = adopting(&[a], vec![g1.node_id], "by-node");
    by_node.parent_node_id = Some(g2.node_id);
    assert_eq!(
        refusal(store.configure_portfolio_node(
            &by_node,
            PortfolioGrantor::Node(g2.node_id),
            "node"
        )),
        MANAGER_NODE_ROOT_OPERATOR_ONLY
    );
    // An edit cannot move a node: it is adopted instead.
    let mut moved = edit_of(&region, "move");
    moved.parent_node_id = None;
    assert_eq!(refusal(create(&store, &moved)), PORTFOLIO_PARENT_IMMUTABLE);
    assert_eq!(snapshot(&store), before);
    assert!(store.portfolio_node_for_seat(seat_p).unwrap().is_none());

    // "Move under": an existing root widens to adopt its sibling root.
    let mut widen = edit_of(&g1, "g1-adopts-g2");
    widen.project_ids = vec![a, b];
    widen.policy = tier_policy(0);
    widen.adopt_node_ids = vec![g2.node_id];
    let host = create(&store, &widen).unwrap();
    assert_eq!(node(&store, g2.node_id).parent_node_id, Some(host.node_id));
    assert_eq!(chain(&store, b), [(0, g1.node_id), (1, g2.node_id)]);
    // A nested node may also be named by a new node inserted under its parent.
    let mut between = level_request("district", seat_x, &[a], Some(&host), 1, "between");
    between.adopt_node_ids = vec![region.node_id];
    let inserted = create(&store, &between).unwrap();
    assert_eq!(inserted.parent_node_id, Some(g1.node_id));
    assert_eq!(
        chain(&store, a),
        [(0, g1.node_id), (1, inserted.node_id), (2, region.node_id)]
    );
}

/// I2 on a parent edit: narrowing a parent that would leave a descendant
/// over-granted is refused atomically, on a five-level chain.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn narrowing_a_parent_below_a_child_is_refused_atomically() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let mut parent: Option<PortfolioNodeV1> = None;
    let mut levels = Vec::new();
    for level in 0..5u16 {
        let seat = session(&store, None);
        let created = create(
            &store,
            &level_request(
                &format!("tier{level}"),
                seat,
                &[a, b],
                parent.as_ref(),
                level,
                &format!("level-{level}"),
            ),
        )
        .unwrap();
        levels.push(created.clone());
        parent = Some(created);
    }
    let middle = &levels[2];
    let before = snapshot(&store);
    let mut lower = edit_of(middle, "lower");
    lower.policy.max_active_sessions = tier_policy(3).max_active_sessions;
    assert_eq!(refusal(create(&store, &lower)), MANAGER_ALLOWANCE_EXCEEDED);
    let mut shrink = edit_of(middle, "shrink");
    shrink.project_ids = vec![a];
    assert_eq!(refusal(create(&store, &shrink)), MANAGER_SCOPE_WIDENED);
    let mut fewer = edit_of(middle, "fewer");
    fewer.policy.capabilities.pop();
    assert_eq!(refusal(create(&store, &fewer)), MANAGER_CAPABILITY_WIDENED);
    // Widening past its own parent is refused too.
    let mut wider = edit_of(middle, "wider");
    wider.policy.max_active_sessions = tier_policy(1).max_active_sessions;
    assert_eq!(refusal(create(&store, &wider)), MANAGER_ALLOWANCE_EXCEEDED);
    assert_eq!(snapshot(&store), before);
    // A legal edit keeps every descendant in place at its depth.
    let mut legal = edit_of(middle, "legal");
    legal.policy.retry_delay_seconds = 30;
    let edited = create(&store, &legal).unwrap();
    assert!(edited.authority_epoch > middle.authority_epoch);
    assert_eq!(
        chain(&store, a)
            .into_iter()
            .map(|(_, id)| id)
            .collect::<Vec<_>>(),
        levels.iter().map(|node| node.node_id).collect::<Vec<_>>()
    );
}

/// Revoke is grantor-scoped (operator decision 2026-10-05): the
/// operator-granted child of the revoked pinnacle re-roots with its seat,
/// epoch and subtree; the pinnacle's own appointee dies with its subtree; the
/// revoked nodes' queued mail is retired.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn revoke_reparents_operator_grants_and_revokes_node_grants_with_their_subtree() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let seats: Vec<Uuid> = (0..5).map(|_| session(&store, None)).collect();
    let pinnacle = create(
        &store,
        &level_request("pinnacle", seats[0], &[a, b], None, 0, "p"),
    )
    .unwrap();
    let global = create(
        &store,
        &level_request("global", seats[1], &[a], Some(&pinnacle), 1, "g"),
    )
    .unwrap();
    let region = create(
        &store,
        &level_request("region", seats[2], &[a], Some(&global), 2, "r"),
    )
    .unwrap();
    // The pinnacle's own appointee over B, and an operator grant beneath it.
    let appointee = store
        .configure_portfolio_node(
            &level_request("global", seats[3], &[b], Some(&pinnacle), 1, "appointee"),
            PortfolioGrantor::Node(pinnacle.node_id),
            "node",
        )
        .unwrap();
    assert_eq!(appointee.grantor, format!("node:{}", pinnacle.node_id));
    let beneath = create(
        &store,
        &level_request("region", seats[4], &[b], Some(&appointee), 2, "beneath"),
    )
    .unwrap();
    // Queued mail to every node's seat.
    let mut jobs = Vec::new();
    for (index, (held, project)) in [(&pinnacle, a), (&global, a), (&appointee, b)]
        .into_iter()
        .enumerate()
    {
        let current = node(&store, held.node_id);
        let receipt = store
            .queue_global_message(&GlobalMessage {
                grant: &current.grant,
                direction: GlobalMessageDirection::ToGlobal,
                project_id: project,
                sender: session(&store, Some(project)),
                target: current.grant.seat_session_id,
                idempotency_key: &format!("m{index}"),
                request: serde_json::json!({"message": index}),
                delivery: format!("report {index}"),
            })
            .unwrap();
        jobs.push(receipt.message_id);
    }
    let before_global = store
        .global_project_principal(seats[1], a)
        .unwrap()
        .unwrap();

    let (revoked, outcome) = store
        .revoke_portfolio_node_outcome(&RevokePortfolioNodeRequestV1 {
            node_id: pinnacle.node_id,
            expected_grant_version: pinnacle.grant.grant_version,
            expected_authority_epoch: pinnacle.authority_epoch,
            idempotency_key: "revoke-p".into(),
        })
        .unwrap();
    assert_eq!(revoked.state, "revoked");
    assert_eq!(
        outcome.revoked,
        [pinnacle.node_id, appointee.node_id, beneath.node_id]
    );
    assert_eq!(outcome.reparented, [global.node_id, region.node_id]);
    for id in &outcome.revoked {
        assert_eq!(node(&store, *id).state, "revoked");
    }
    // The global re-roots with its seat, epoch and ledger principal intact;
    // its region moves up with it.
    let rerooted = node(&store, global.node_id);
    assert_eq!(rerooted.state, "active");
    assert_eq!(rerooted.parent_node_id, None);
    assert_eq!(rerooted.grantor, "operator");
    assert_eq!(rerooted.authority_epoch, global.authority_epoch);
    assert_eq!(rerooted.grant.seat_session_id, seats[1]);
    assert_eq!(chain(&store, a), [(0, global.node_id), (1, region.node_id)]);
    assert!(chain(&store, b).is_empty());
    let after_global = store
        .global_project_principal(seats[1], a)
        .unwrap()
        .unwrap();
    assert_eq!(after_global.authority_epoch, before_global.authority_epoch);
    assert_eq!(after_global.seat_root, before_global.seat_root);
    for (seat, project) in [(seats[0], a), (seats[3], b), (seats[4], b)] {
        assert!(
            store
                .global_project_principal(seat, project)
                .unwrap()
                .is_none()
        );
    }
    // Stale mail is retired: the revoked nodes' and the moved grant's.
    for job in jobs {
        assert!(!store.get_scheduled_job(&job).unwrap().unwrap().enabled);
    }
    // A replay at the same versions returns the revoked node.
    let replay = store
        .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
            node_id: pinnacle.node_id,
            expected_grant_version: pinnacle.grant.grant_version,
            expected_authority_epoch: pinnacle.authority_epoch,
            idempotency_key: "revoke-p".into(),
        })
        .unwrap();
    assert_eq!(replay.state, "revoked");
    assert_eq!(node(&store, global.node_id).state, "active");
}

/// The S2 isolation invariants at depth 3: two sibling nodes under one
/// parent never reach each other's seat or project, and no node reaches an
/// ancestor's seat hosted in its own project, to read or to mutate. The
/// parent reaches its own leaf; a descendant never mutates it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn depth_three_siblings_and_ancestors_stay_out_of_reach() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let group = hosted(&store, a, None, SessionKind::Group);
    let epic = hosted(&store, a, Some(group), SessionKind::Epic);
    let seat = |store: &Store| hosted(store, a, Some(epic), SessionKind::Task);
    let seats: Vec<Uuid> = (0..5).map(|_| seat(&store)).collect();
    let swarm = create(
        &store,
        &level_request("swarm", seats[0], &[a, b], None, 0, "s"),
    )
    .unwrap();
    let pinnacle = create(
        &store,
        &level_request("pinnacle", seats[1], &[a, b], Some(&swarm), 1, "p"),
    )
    .unwrap();
    let global = create(
        &store,
        &level_request("global", seats[2], &[a, b], Some(&pinnacle), 2, "g"),
    )
    .unwrap();
    // Two regions leave the global one active session of its 15 for a
    // third, zero-allowance probe.
    let region = |seat, projects: &[Uuid], key: &str| {
        let mut request = level_request("region", seat, projects, Some(&global), 3, key);
        request.policy.max_active_sessions = 6;
        create(&store, &request).unwrap()
    };
    let left = region(seats[3], &[a], "left");
    let right = region(seats[4], &[b], "right");
    assert_eq!(chain(&store, b).last(), Some(&(3, right.node_id)));
    // A third sibling over A at depth 3 overlaps the left region (it asks
    // for no allowance, so the siblings' sum does not refuse it first).
    let extra = seat(&store);
    let mut overlap = level_request("region", extra, &[a], Some(&global), 3, "overlap");
    overlap.policy.max_created_containers = 0;
    overlap.policy.max_created_sessions = 0;
    overlap.policy.max_active_sessions = 1;
    assert_eq!(refusal(create(&store, &overlap)), MANAGER_SCOPE_OVERLAP);
    // The left region never acts in its sibling's project.
    assert_eq!(
        refusal(store.global_project_principal(seats[3], b)),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
    let leaf = seat(&store);
    let principal = store
        .global_project_principal(seats[3], a)
        .unwrap()
        .unwrap();
    let acting = store
        .global_manager_authority_for(&principal, seats[3])
        .unwrap()
        .config;
    // Every other node's seat is out of reach: the sibling and all ancestors.
    for other in [seats[0], seats[1], seats[2], seats[4]] {
        assert!(
            store
                .manager_target_owned_by_ancestor(&acting, other)
                .unwrap(),
            "{other}"
        );
        for mutation in [true, false] {
            assert!(
                store
                    .manager_session_control_scope(seats[3], other, mutation)
                    .unwrap()
                    .is_none(),
                "mutation={mutation}"
            );
        }
    }
    assert!(
        !store
            .manager_target_owned_by_ancestor(&acting, leaf)
            .unwrap()
    );
    for mutation in [true, false] {
        assert!(
            store
                .manager_session_control_scope(seats[3], leaf, mutation)
                .unwrap()
                .is_some()
        );
    }
    // The ancestors never reach a descendant's seat either (S5 adds direct
    // child seat control), but they reach the shared project's leaves.
    for ancestor in [seats[0], seats[1], seats[2]] {
        assert!(
            store
                .manager_session_control_scope(ancestor, seats[3], false)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .manager_session_control_scope(ancestor, leaf, true)
                .unwrap()
                .is_some()
        );
    }
    let _ = right;
}

/// A context-cap seat transfer of a nested node keeps its depth, epoch and
/// ledger principal (the coverage is rebuilt at the node's own depth).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_nested_seat_transfer_keeps_its_depth_and_principal() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let (seat_p, seat_g) = (session(&store, None), session(&store, None));
    let pinnacle = create(
        &store,
        &level_request("pinnacle", seat_p, &[a], None, 0, "p"),
    )
    .unwrap();
    let global = create(
        &store,
        &level_request("global", seat_g, &[a], Some(&pinnacle), 1, "g"),
    )
    .unwrap();
    let before = store.global_project_principal(seat_g, a).unwrap().unwrap();
    let successor = super::tests::successor_of(&store, seat_g);
    assert!(store.transfer_global_seat(seat_g, successor).unwrap());
    assert_eq!(
        chain(&store, a),
        [(0, pinnacle.node_id), (1, global.node_id)]
    );
    let moved = node(&store, global.node_id);
    assert_eq!(moved.parent_node_id, Some(pinnacle.node_id));
    assert_eq!(moved.authority_epoch, global.authority_epoch);
    let after = store
        .global_project_principal(successor, a)
        .unwrap()
        .unwrap();
    assert_eq!(after.authority_epoch, before.authority_epoch);
    assert_eq!(after.seat_root, before.seat_root);
}

/// Swarm → pinnacle → global over `projects`, each from `tier_policy`.
fn three_levels(store: &Store, projects: &[Uuid]) -> ([Uuid; 3], [PortfolioNodeV1; 3]) {
    let seats = [
        session(store, None),
        session(store, None),
        session(store, None),
    ];
    let swarm = create(
        store,
        &level_request("swarm", seats[0], projects, None, 0, "swarm"),
    )
    .unwrap();
    let pinnacle = create(
        store,
        &level_request("pinnacle", seats[1], projects, Some(&swarm), 1, "pinnacle"),
    )
    .unwrap();
    let global = create(
        store,
        &level_request("global", seats[2], projects, Some(&pinnacle), 2, "global"),
    )
    .unwrap();
    (seats, [swarm, pinnacle, global])
}

/// The V2 ledger config `seat` acts under in `project`.
fn acting_config(
    store: &Store,
    seat: Uuid,
    project: Uuid,
) -> rsi_common::harness_manager::HarnessManagerConfigV1 {
    let principal = store
        .global_project_principal(seat, project)
        .unwrap()
        .unwrap();
    store
        .global_manager_authority_for(&principal, seat)
        .unwrap()
        .config
}

/// Journal `count` queued `create_session` reservations for `config`.
fn reserve_sessions(
    store: &Store,
    config: &rsi_common::harness_manager::HarnessManagerConfigV1,
    count: usize,
) {
    for index in 0..count {
        store
            .seed_manager_action_for_test(
                config,
                ManagerActionV2::CreateSession {
                    parent_id: Uuid::new_v4(),
                    kind: SessionKind::Task,
                    query: format!("worker {index}"),
                    launch: super::tests::launch(),
                    sandbox_source: None,
                },
                ManagerActionStateV2::Queued,
                None,
            )
            .unwrap();
    }
}

/// #1301, plan §2.3(d): work is charged once, to the originating node and
/// its ancestors. At depth three, with every node created before any
/// admission, the swarm's own reservations never count against the global
/// below it, the global's count against the pinnacle and the swarm, and a
/// project-level principal's count against all three.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn creation_budgets_charge_the_origin_and_its_ancestors_never_a_descendant() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seats = [
        session(&store, None),
        session(&store, None),
        session(&store, None),
    ];
    let mut nodes: Vec<PortfolioNodeV1> = Vec::new();
    for (depth, (label, cap)) in [("swarm", 9), ("pinnacle", 5), ("global", 2)]
        .into_iter()
        .enumerate()
    {
        let mut request =
            level_request(label, seats[depth], &[a], nodes.last(), depth as u16, label);
        request.policy.max_created_sessions = cap;
        nodes.push(create(&store, &request).unwrap());
    }
    let configs: Vec<_> = seats
        .iter()
        .map(|seat| acting_config(&store, *seat, a))
        .collect();
    let allowance = |config| store.manager_ancestor_creation_allowance(config, false, 1, 0);
    // The swarm reserves two launches: the global (cap 2) still has both.
    reserve_sessions(&store, &configs[0], 2);
    allowance(&configs[2]).unwrap();
    allowance(&configs[1]).unwrap();
    // The global's own two fill its cap and charge both ancestors.
    reserve_sessions(&store, &configs[2], 2);
    assert_eq!(refusal(allowance(&configs[2])), "manager_v2_creation_limit");
    // The pinnacle holds the global's 2 and its own 2 of its 5: one more
    // fits, two do not. The swarm holds 6 of its 9.
    reserve_sessions(&store, &configs[1], 2);
    allowance(&configs[1]).unwrap();
    assert_eq!(
        refusal(store.manager_ancestor_creation_allowance(&configs[1], false, 2, 0)),
        "manager_v2_creation_limit"
    );
    allowance(&configs[0]).unwrap();
    // A project-level principal under the chain is charged to every node:
    // its first launch is refused by the full global.
    let pm = rsi_common::harness_manager::HarnessManagerConfigV1 {
        manager_session_id: session(&store, Some(a)),
        row_version: 1,
        current_session_id: None,
        ..configs[2].clone()
    };
    assert_eq!(
        refusal(allowance(&pm)),
        rsi_common::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED
    );
    reserve_sessions(&store, &pm, 1);
    // The swarm now holds 2 + 2 + 2 + 1 = 7 of 9: one more fits, two do not.
    store
        .manager_ancestor_creation_allowance(&configs[0], false, 2, 0)
        .unwrap();
    assert_eq!(
        refusal(store.manager_ancestor_creation_allowance(&configs[0], false, 3, 0)),
        "manager_v2_creation_limit"
    );
    let _ = nodes;
}

/// #1302, plan §2.2: the finite allowances of a parent's active children sum
/// below the parent's (spend may reach it), at depth three, on a create (by
/// the operator and by a node grantor, S5's appointment path), on an edit of
/// a child and on an adoption.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn sibling_allowances_sum_below_their_parent() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c, d) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
        project(&store, "D"),
    );
    let (_, [_, _, global]) = three_levels(&store, &[a, b]);
    // The global holds 31 sessions; each region alone narrows it.
    let region = |seat, projects: &[Uuid], sessions, key: &str| {
        let mut request = level_request("region", seat, projects, Some(&global), 3, key);
        request.policy.max_created_sessions = sessions;
        request
    };
    let left = create(&store, &region(session(&store, None), &[a], 15, "left")).unwrap();
    let before = snapshot(&store);
    let crowded = region(session(&store, None), &[b], 16, "right-16");
    assert_eq!(
        refusal(create(&store, &crowded)),
        MANAGER_ALLOWANCE_EXCEEDED
    );
    assert_eq!(
        refusal(store.configure_portfolio_node(
            &crowded,
            PortfolioGrantor::Node(global.node_id),
            "node"
        )),
        MANAGER_ALLOWANCE_EXCEEDED
    );
    assert_eq!(snapshot(&store), before);
    let right = create(&store, &region(session(&store, None), &[b], 15, "right")).unwrap();
    assert_eq!(right.parent_node_id, Some(global.node_id));
    // Raising one sibling into the other's share is refused; lowering is not.
    let mut raise = edit_of(&left, "left-raise");
    raise.policy.max_created_sessions = 16;
    assert_eq!(refusal(create(&store, &raise)), MANAGER_ALLOWANCE_EXCEEDED);
    let mut lower = edit_of(&left, "left-lower");
    lower.policy.max_created_sessions = 14;
    create(&store, &lower).unwrap();
    // Adoption: two roots of 10 sessions each need more than 20 above them.
    let roots: Vec<PortfolioNodeV1> = [(c, "c"), (d, "d")]
        .into_iter()
        .map(|(project, key)| {
            let mut request =
                level_request("global", session(&store, None), &[project], None, 2, key);
            request.policy.max_created_sessions = 10;
            create(&store, &request).unwrap()
        })
        .collect();
    let adopting = |sessions, key: &str| {
        let mut request = level_request("pinnacle", session(&store, None), &[c, d], None, 1, key);
        request.policy.max_created_sessions = sessions;
        request.adopt_node_ids = roots.iter().map(|root| root.node_id).collect();
        request
    };
    let before = snapshot(&store);
    assert_eq!(
        refusal(create(&store, &adopting(20, "adopt-20"))),
        MANAGER_ALLOWANCE_EXCEEDED
    );
    assert_eq!(snapshot(&store), before);
    let host = create(&store, &adopting(21, "adopt-21")).unwrap();
    for root in &roots {
        assert_eq!(
            node(&store, root.node_id).parent_node_id,
            Some(host.node_id)
        );
    }
    // The host cannot then shrink below its adopted children's sum.
    let mut shrink = edit_of(&node(&store, host.node_id), "host-shrink");
    shrink.policy.max_created_sessions = 20;
    assert_eq!(refusal(create(&store, &shrink)), MANAGER_ALLOWANCE_EXCEEDED);
}

/// #1303: the adoption set is part of a configure's replay identity. At depth
/// three, a same-key request naming another adoption set is a conflict and
/// moves nothing; the exact replay returns the node and writes nothing.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_same_key_configure_with_another_adoption_set_conflicts() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (_, [_, pinnacle, global]) = three_levels(&store, &[a, b]);
    // A second child of the pinnacle, beside the global: the global narrows
    // to A first so B is free at depth two.
    let mut narrow = edit_of(&global, "global-a");
    narrow.project_ids = vec![a];
    narrow.policy = tier_policy(3);
    let global = create(&store, &narrow).unwrap();
    let sibling = create(
        &store,
        &level_request(
            "global",
            session(&store, None),
            &[b],
            Some(&pinnacle),
            2,
            "sibling",
        ),
    )
    .unwrap();
    // The district takes the global's place beside the sibling: above the
    // global, and with the sibling still under the pinnacle's allowance.
    let mut district = level_request(
        "district",
        session(&store, None),
        &[a],
        Some(&pinnacle),
        2,
        "district",
    );
    district.policy.max_created_containers = 14;
    district.policy.max_created_sessions = 30;
    district.policy.max_active_sessions = 14;
    district.adopt_node_ids = vec![global.node_id];
    let inserted = create(&store, &district).unwrap();
    assert_eq!(
        node(&store, global.node_id).parent_node_id,
        Some(inserted.node_id)
    );
    let before = snapshot(&store);
    let mut wider = district.clone();
    wider.adopt_node_ids = vec![global.node_id, sibling.node_id];
    assert_eq!(
        refusal(create(&store, &wider)),
        PORTFOLIO_IDEMPOTENCY_CONFLICT
    );
    let mut none = district.clone();
    none.adopt_node_ids.clear();
    assert_eq!(
        refusal(create(&store, &none)),
        PORTFOLIO_IDEMPOTENCY_CONFLICT
    );
    assert_eq!(snapshot(&store), before);
    assert_eq!(
        node(&store, sibling.node_id).parent_node_id,
        Some(pinnacle.node_id)
    );
    assert_eq!(create(&store, &district).unwrap().node_id, inserted.node_id);
    assert_eq!(snapshot(&store), before);
}

/// #1305, plan §9 Q2: a revoke always re-parents operator-granted children,
/// even past the new parent's direct-report cap. At depth three the revoke
/// succeeds and names the over-capacity parent; that parent appoints nothing
/// new and no edit adds to the overflow until it is back under, while an
/// edit that does not add to it still lands.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_revoke_reparents_past_the_parents_cap_and_blocks_new_appointments() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c, d) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
        project(&store, "D"),
    );
    let mut top = level_request(
        "swarm",
        session(&store, None),
        &[a, b, c, d],
        None,
        0,
        "swarm",
    );
    top.max_direct_reports = 2;
    let swarm = create(&store, &top).unwrap();
    let middle = |projects: &[Uuid], key: &str| {
        let mut request = level_request(
            "pinnacle",
            session(&store, None),
            projects,
            Some(&swarm),
            1,
            key,
        );
        request.max_direct_reports = 2;
        create(&store, &request).unwrap()
    };
    let pinnacle = middle(&[a, b], "pinnacle");
    let other = middle(&[c], "other");
    // Two operator-granted globals under the pinnacle.
    let globals: Vec<PortfolioNodeV1> = [(a, "ga"), (b, "gb")]
        .into_iter()
        .map(|(project, key)| {
            let mut request = level_request(
                "global",
                session(&store, None),
                &[project],
                Some(&pinnacle),
                2,
                key,
            );
            request.max_direct_reports = 2;
            create(&store, &request).unwrap()
        })
        .collect();
    let (_, outcome) = store
        .revoke_portfolio_node_outcome(&RevokePortfolioNodeRequestV1 {
            node_id: pinnacle.node_id,
            expected_grant_version: pinnacle.grant.grant_version,
            expected_authority_epoch: pinnacle.authority_epoch,
            idempotency_key: "revoke-pinnacle".into(),
        })
        .unwrap();
    assert_eq!(outcome.revoked, [pinnacle.node_id]);
    assert_eq!(
        outcome.reparented,
        globals.iter().map(|g| g.node_id).collect::<Vec<_>>()
    );
    assert_eq!(outcome.over_capacity, [swarm.node_id]);
    for global in &globals {
        assert_eq!(
            node(&store, global.node_id).parent_node_id,
            Some(swarm.node_id)
        );
    }
    // Over its cap, the swarm appoints nothing new, by either grantor.
    let mut fresh = level_request(
        "global",
        session(&store, None),
        &[d],
        Some(&swarm),
        2,
        "fresh",
    );
    fresh.max_direct_reports = 2;
    assert_eq!(refusal(create(&store, &fresh)), MANAGER_ALLOWANCE_EXCEEDED);
    assert_eq!(
        refusal(store.configure_portfolio_node(
            &fresh,
            PortfolioGrantor::Node(swarm.node_id),
            "node"
        )),
        MANAGER_ALLOWANCE_EXCEEDED
    );
    // An edit that adds nothing to the overflow still lands.
    let mut keep = edit_of(&node(&store, swarm.node_id), "swarm-keep");
    keep.policy.retry_delay_seconds = 30;
    let swarm = create(&store, &keep).unwrap();
    // Back under its cap (and its allowance), it appoints again.
    for gone in [globals[1].node_id, other.node_id] {
        let gone = node(&store, gone);
        store
            .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
                node_id: gone.node_id,
                expected_grant_version: gone.grant.grant_version,
                expected_authority_epoch: gone.authority_epoch,
                idempotency_key: format!("revoke-{}", gone.node_id),
            })
            .unwrap();
    }
    let mut again = fresh.clone();
    again.idempotency_key = "fresh-again".into();
    let appointed = create(&store, &again).unwrap();
    assert_eq!(appointed.parent_node_id, Some(swarm.node_id));
}
