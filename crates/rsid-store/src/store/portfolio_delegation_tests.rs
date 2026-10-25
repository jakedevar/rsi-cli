//! Store tests for delegation at every portfolio level (#1239, hierarchy S5,
//! M3): appoint, replace and revoke a child PM or child portfolio node.

use super::tests::{code, create, edit_of, launch, project, refusal, root, session};
use super::*;
use crate::store::harness_manager_v2::ManagerCallerV1;
use crate::store::portfolio_nodes::delegation::{ChildAppointment, ChildTargetRef};
use crate::test_support::test_session;
use rsi_common::global_manager::{
    GLOBAL_LAUNCH_NOT_ALLOWED, GLOBAL_MANAGER_NOT_SEAT, MANAGER_PROJECT_NOT_IN_SCOPE,
};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{
    ManagerCapabilityV2, ManagerFenceV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2,
};
use rsi_common::manager_tree::ManagerTreeKindV1;
use rsi_common::portfolio_delegation::{
    AgentManagerAppointChildRequestV1, AgentManagerRevokeChildRequestV1, AppointChildTargetV1,
    MANAGER_CHILD_OPERATOR_GRANTED, MANAGER_DIRECT_REPORT_CAP, MANAGER_NODE_NOT_IN_SCOPE,
    MANAGER_SCOPE_NOT_NARROWED,
};
use rsi_common::portfolio_nodes::{
    MANAGER_ALLOWANCE_EXCEEDED, MANAGER_CAPABILITY_WIDENED, PORTFOLIO_IDEMPOTENCY_CONFLICT,
};
use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};
use std::path::PathBuf;

/// The PM verb set at every level; each finite allowance lower per level (the
/// narrowing rule keeps capabilities equal), with sibling sums below the parent.
fn tier_policy(level: u16) -> ManagerPolicyV2 {
    // (containers, created, active) per level: five siblings at level 1
    // and two at levels 2 and 3 still sum below their parent (#1302), inside
    // the policy bounds (64, 1024, 1..=100).
    let (containers, created, active) =
        [(60, 1000, 96), (11, 190, 18), (5, 60, 6), (2, 20, 2)][usize::from(level)];
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![
            ManagerCapabilityV2::IssueCoordinate,
            ManagerCapabilityV2::SessionCreate,
            ManagerCapabilityV2::SessionControl,
        ],
        max_created_containers: containers,
        max_created_sessions: created,
        max_active_sessions: active,
        ..ManagerPolicyV2::default()
    }
}

/// An operator-granted node at `level` (a root when `parent` is `None`).
fn operator_node(
    store: &Store,
    label: &str,
    projects: &[Uuid],
    parent: Option<&PortfolioNodeV1>,
    level: u16,
) -> (PortfolioNodeV1, Uuid) {
    // This policy fixture exercises metered budgets.
    store
        .set_daemon_setting("claude_billing_mode", "metered_api")
        .unwrap();
    store
        .set_daemon_setting("codex_billing_mode", "metered_api")
        .unwrap();
    let seat = session(store, None);
    let mut request = root(label, seat, projects, &format!("op-{label}-{seat}"));
    request.parent_node_id = parent.map(|parent| parent.node_id);
    request.policy = tier_policy(level);
    (create(store, &request).unwrap(), seat)
}

fn appoint_request(target: AppointChildTargetV1, key: &str) -> AgentManagerAppointChildRequestV1 {
    AgentManagerAppointChildRequestV1 {
        target,
        launch: launch(),
        query: "You manage your coverage. Call AgentGetAuthorityCatalog {} first.".into(),
        idempotency_key: key.into(),
        sandbox: Some(false),
        handoff: None,
    }
}

fn child_target(label: &str, projects: &[Uuid], level: u16) -> AppointChildTargetV1 {
    AppointChildTargetV1::Portfolio {
        node_id: None,
        tier_label: Some(label.into()),
        project_ids: projects.to_vec(),
        allowed_launches: Vec::new(),
        policy: Some(tier_policy(level)),
        child_policy: None,
        max_direct_reports: None,
        launch_project_id: None,
        expected_grant_version: None,
    }
}

fn seat_target(node: Uuid) -> AppointChildTargetV1 {
    AppointChildTargetV1::Portfolio {
        node_id: Some(node),
        tier_label: None,
        project_ids: Vec::new(),
        allowed_launches: Vec::new(),
        policy: None,
        child_policy: None,
        max_direct_reports: None,
        launch_project_id: None,
        expected_grant_version: None,
    }
}

/// The launch a real appointment makes: the reserved id, persisted.
fn launched(store: &Store, id: Uuid, project: Uuid) {
    let mut row = test_session(id, PathBuf::from("/tmp/portfolio"));
    row.project_id = Some(project);
    row.session_kind = SessionKind::Standard;
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
}

/// Admission, the launch and the appointment, as the daemon runs them.
fn appoint(
    store: &Store,
    caller: Uuid,
    request: &AgentManagerAppointChildRequestV1,
) -> Result<(ChildAppointment, (i64, i64))> {
    let appointment = store.begin_child_appointment(caller, request)?;
    if store.get_session(appointment.session_id)?.is_none() {
        launched(store, appointment.session_id, appointment.launch_project_id);
    }
    let versions = match appointment.target {
        ChildTargetRef::Project(_) => {
            store.finish_project_appointment(caller, request, &appointment)?
        }
        ChildTargetRef::Portfolio(_) => {
            store.finish_portfolio_appointment(caller, request, &appointment)?
        }
    };
    Ok((appointment, versions))
}

fn appoint_child(
    store: &Store,
    caller: Uuid,
    label: &str,
    projects: &[Uuid],
    level: u16,
) -> (PortfolioNodeV1, Uuid) {
    // This policy fixture exercises metered budgets.
    store
        .set_daemon_setting("claude_billing_mode", "metered_api")
        .unwrap();
    store
        .set_daemon_setting("codex_billing_mode", "metered_api")
        .unwrap();
    let request = appoint_request(
        child_target(label, projects, level),
        &format!("child-{label}"),
    );
    let (appointment, _) = appoint(store, caller, &request).unwrap();
    let ChildTargetRef::Portfolio(node) = appointment.target else {
        panic!("a child node target");
    };
    (
        store.get_portfolio_node(node).unwrap().unwrap(),
        appointment.session_id,
    )
}

fn count(store: &Store, table: &str) -> i64 {
    // sql-dynamic-ok: test helper over static table names.
    store
        .conn
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        }) // sql-dynamic-ok
        .unwrap()
}

fn chain(store: &Store, project: Uuid) -> Vec<(u16, Uuid)> {
    store
        .portfolio_chain_for_project(project)
        .unwrap()
        .into_iter()
        .map(|row| (row.depth, row.node_id))
        .collect()
}

fn revoke(node: &PortfolioNodeV1, key: &str) -> AgentManagerRevokeChildRequestV1 {
    AgentManagerRevokeChildRequestV1 {
        node_id: node.node_id,
        expected_grant_version: node.grant.grant_version,
        idempotency_key: key.into(),
    }
}

fn principal(store: &Store, seat: Uuid, project: Uuid) -> Result<(Uuid, i64)> {
    match store.resolve_manager_caller(seat, Some(project))? {
        ManagerCallerV1::Global(authority) => Ok((
            authority.config.manager_session_id,
            authority.config.row_version,
        )),
        other => panic!("expected the portfolio arm, got {other:?}"),
    }
}

/// Issue AC1: a global over [A, B, C] creates a child node over [B, C]. The
/// child holds the PM set in B and is refused in A; a second child over [C]
/// overlaps, and children over all of A, B, C or outside the coverage are
/// refused, each before any appointment row or session exists.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_global_creates_a_narrower_child_and_siblings_stay_disjoint() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c, d) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
        project(&store, "D"),
    );
    let (global, global_seat) = operator_node(&store, "global", &[a, b, c], None, 0);
    let (child, child_seat) = appoint_child(&store, global_seat, "area-lead", &[b, c], 1);
    assert_eq!(child.parent_node_id, Some(global.node_id));
    assert_eq!(child.grantor, format!("node:{}", global.node_id));
    assert_eq!(child.grant.seat_session_id, child_seat);
    assert_eq!(child.grant.allowed_launches, vec![launch()]);
    assert_eq!(chain(&store, b), [(0, global.node_id), (1, child.node_id)]);
    assert_eq!(chain(&store, a), [(0, global.node_id)]);
    // The child can act (and launch workers) in B, and is refused in A.
    let fence = ManagerFenceV2 {
        scope_version: child.authority_epoch,
        policy_version: child.grant.grant_version,
    };
    store
        .manager_v2_authorize_in(
            child_seat,
            Some(b),
            &fence,
            Some(ManagerCapabilityV2::SessionCreate),
        )
        .unwrap();
    assert_eq!(
        principal(&store, child_seat, b).unwrap(),
        (child_seat, child.authority_epoch)
    );
    assert_eq!(
        refusal(store.resolve_manager_caller(child_seat, Some(a))),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
    let (sessions, rows) = (
        count(&store, "sessions"),
        count(&store, "manager_portfolio_appointments"),
    );
    for (target, expected) in [
        (child_target("second", &[c], 1), MANAGER_SCOPE_OVERLAP),
        (
            child_target("whole", &[a, b, c], 1),
            MANAGER_SCOPE_NOT_NARROWED,
        ),
        (
            child_target("outside", &[d], 1),
            MANAGER_PROJECT_NOT_IN_SCOPE,
        ),
        (
            AppointChildTargetV1::Project { project_id: d },
            MANAGER_PROJECT_NOT_IN_SCOPE,
        ),
    ] {
        let request = appoint_request(target, &format!("refused-{expected}"));
        assert_eq!(
            refusal(store.begin_child_appointment(global_seat, &request)),
            expected
        );
    }
    assert_eq!(
        count(&store, "sessions"),
        sessions,
        "no session was created"
    );
    assert_eq!(
        count(&store, "manager_portfolio_appointments"),
        rows,
        "no appointment was recorded"
    );
    // A non-seat, and the child's own token for a target outside its
    // coverage, are refused too.
    let stranger = session(&store, None);
    assert_eq!(
        refusal(store.begin_child_appointment(
            stranger,
            &appoint_request(child_target("x", &[b], 1), "stranger")
        )),
        GLOBAL_MANAGER_NOT_SEAT
    );
    assert_eq!(
        refusal(store.begin_child_appointment(
            child_seat,
            &appoint_request(child_target("x", &[a], 2), "child-a")
        )),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
}

/// Issue AC3: a disallowed launch, a widened capability, launch or
/// allowance, and a sixth direct report are refused before any session or
/// appointment row exists. Live PMs of the projects a node covers deepest
/// count as direct reports.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn refusals_happen_before_any_session_and_the_cap_counts_pms() {
    let store = Store::open_in_memory().unwrap();
    let projects: Vec<Uuid> = (0..7).map(|n| project(&store, &format!("P{n}"))).collect();
    let (_, seat) = operator_node(&store, "global", &projects, None, 0);
    let before = (
        count(&store, "sessions"),
        count(&store, "manager_portfolio_appointments"),
    );
    let mut disallowed = appoint_request(child_target("c", &projects[..1], 1), "launch");
    disallowed.launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Codex,
        model: "gpt-6-astra".into(),
        effort: None,
    };
    let mut capability = child_target("c", &projects[..1], 1);
    if let AppointChildTargetV1::Portfolio { policy, .. } = &mut capability {
        policy
            .as_mut()
            .unwrap()
            .capabilities
            .push(ManagerCapabilityV2::Deploy);
    }
    let mut launches = child_target("c", &projects[..1], 1);
    if let AppointChildTargetV1::Portfolio {
        allowed_launches, ..
    } = &mut launches
    {
        allowed_launches.push(ManagerLaunchChoiceV2 {
            provider: SessionProvider::Codex,
            model: "gpt-6-astra".into(),
            effort: None,
        });
    }
    for (request, expected) in [
        (disallowed, GLOBAL_LAUNCH_NOT_ALLOWED),
        (
            appoint_request(capability, "capability"),
            MANAGER_CAPABILITY_WIDENED,
        ),
        (
            appoint_request(launches, "launches"),
            MANAGER_CAPABILITY_WIDENED,
        ),
        (
            // Equal allowances: a child must hold strictly less.
            appoint_request(child_target("c", &projects[..1], 0), "allowance"),
            MANAGER_ALLOWANCE_EXCEEDED,
        ),
    ] {
        assert_eq!(
            refusal(store.begin_child_appointment(seat, &request)),
            expected
        );
    }
    assert_eq!(
        (
            count(&store, "sessions"),
            count(&store, "manager_portfolio_appointments")
        ),
        before
    );
    // Four children and one PM fill the default cap of five.
    for (index, project) in projects[..4].iter().enumerate() {
        appoint_child(&store, seat, &format!("child{index}"), &[*project], 1);
    }
    appoint(
        &store,
        seat,
        &appoint_request(
            AppointChildTargetV1::Project {
                project_id: projects[4],
            },
            "pm-4",
        ),
    )
    .unwrap();
    let before = (
        count(&store, "sessions"),
        count(&store, "manager_portfolio_appointments"),
    );
    for (target, key) in [
        (child_target("sixth", &projects[5..6], 1), "sixth-node"),
        (
            AppointChildTargetV1::Project {
                project_id: projects[6],
            },
            "sixth-pm",
        ),
    ] {
        assert_eq!(
            refusal(store.begin_child_appointment(seat, &appoint_request(target, key))),
            MANAGER_DIRECT_REPORT_CAP
        );
    }
    assert_eq!(
        (
            count(&store, "sessions"),
            count(&store, "manager_portfolio_appointments")
        ),
        before
    );
    // Replacing an existing PM adds no report and is admitted.
    appoint(
        &store,
        seat,
        &appoint_request(
            AppointChildTargetV1::Project {
                project_id: projects[4],
            },
            "pm-4-replace",
        ),
    )
    .unwrap();
}

/// Issue AC2: replacing the PM of A through AppointChild leaves the node's
/// child policy saved and live, as in v0; a replay returns the same session
/// and a different request under the key is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn replacing_a_pm_saves_the_child_policy_and_replays() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let seat = session(&store, None);
    let mut request = root("global", seat, &[a, b], "g");
    request.policy = tier_policy(0);
    let child_policy = ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![ManagerCapabilityV2::IssueCoordinate],
        ..ManagerPolicyV2::default()
    };
    request.child_policy = Some(child_policy.clone());
    create(&store, &request).unwrap();
    // The operator's PM of A.
    let old_pm = session(&store, Some(a));
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: a,
            session_id: old_pm,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .unwrap();
    let request = appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a");
    let (appointment, versions) = appoint(&store, seat, &request).unwrap();
    let config = store.get_harness_manager(a).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(appointment.session_id));
    let policy = store.get_harness_manager_policy(a).unwrap().unwrap();
    assert!(!policy.revoked);
    // #1412: the handed policy is the child policy as configured; an empty
    // launch list stays "inherit" and is resolved against the node live.
    assert_eq!(policy.policy, child_policy);
    assert_eq!(versions, (policy.scope_version, policy.row_version));
    let replay = store.begin_child_appointment(seat, &request).unwrap();
    assert_eq!(replay.session_id, appointment.session_id);
    assert_eq!(replay.appointed, Some(versions));
    assert!(replay.replayed);
    let mut changed = request.clone();
    changed.query = "Another prompt.".into();
    assert_eq!(
        refusal(store.begin_child_appointment(seat, &changed)),
        PORTFOLIO_IDEMPOTENCY_CONFLICT
    );
    assert_eq!(count(&store, "manager_portfolio_appointments"), 1);
    assert_eq!(
        store
            .project_manager_grantor(a, appointment.session_id)
            .unwrap(),
        format!("node:{}", store.portfolio_seat_node(seat).unwrap().unwrap())
    );
    assert_eq!(
        store.project_manager_grantor(b, Uuid::new_v4()).unwrap(),
        "operator"
    );
}

/// Issue AC4 at depth 3: a node revokes a child it granted with that
/// child's node-granted subtree, operator-granted descendants re-parent, and
/// an operator-granted child, a sibling, an ancestor and another node's
/// grant are refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn revoke_child_is_grantor_scoped_at_depth_three() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c, d) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
        project(&store, "D"),
    );
    let (pinnacle, pinnacle_seat) = operator_node(&store, "pinnacle", &[a, b, c, d], None, 0);
    let (global, global_seat) = operator_node(&store, "global", &[a, b, c], Some(&pinnacle), 1);
    let (sibling, _) = operator_node(&store, "global", &[d], Some(&pinnacle), 1);
    let (x, x_seat) = appoint_child(&store, global_seat, "area-x", &[b, c], 2);
    let (x2, _) = appoint_child(&store, global_seat, "area-x2", &[a], 2);
    let (y, y_seat) = appoint_child(&store, x_seat, "team-y", &[c], 3);
    let (z, z_seat) = operator_node(&store, "team-z", &[b], Some(&x), 3);
    assert_eq!(
        chain(&store, c),
        [
            (0, pinnacle.node_id),
            (1, global.node_id),
            (2, x.node_id),
            (3, y.node_id)
        ]
    );
    for (caller, target, expected) in [
        (y_seat, &x, MANAGER_NODE_NOT_IN_SCOPE),
        (x_seat, &x2, MANAGER_NODE_NOT_IN_SCOPE),
        (global_seat, &sibling, MANAGER_NODE_NOT_IN_SCOPE),
        (global_seat, &pinnacle, MANAGER_NODE_NOT_IN_SCOPE),
        (global_seat, &global, MANAGER_NODE_NOT_IN_SCOPE),
        (global_seat, &y, MANAGER_NODE_NOT_IN_SCOPE),
        (global_seat, &z, MANAGER_CHILD_OPERATOR_GRANTED),
        (x_seat, &z, MANAGER_CHILD_OPERATOR_GRANTED),
        (pinnacle_seat, &global, MANAGER_CHILD_OPERATOR_GRANTED),
    ] {
        assert_eq!(
            refusal(store.revoke_child_portfolio_node(caller, &revoke(target, "r"))),
            expected,
            "{} on {}",
            caller,
            target.tier_label
        );
    }
    let mut stale = revoke(&x, "stale");
    stale.expected_grant_version += 100;
    assert_eq!(
        refusal(store.revoke_child_portfolio_node(global_seat, &stale)),
        MANAGER_NODE_STALE
    );
    let (view, outcome, deduplicated) = store
        .revoke_child_portfolio_node(global_seat, &revoke(&x, "revoke-x"))
        .unwrap();
    assert_eq!(view.state, "revoked");
    assert!(!deduplicated);
    assert_eq!(outcome.revoked, vec![x.node_id, y.node_id]);
    assert_eq!(outcome.reparented, vec![z.node_id]);
    let moved = store.get_portfolio_node(z.node_id).unwrap().unwrap();
    assert_eq!(moved.state, "active");
    assert_eq!(moved.parent_node_id, Some(global.node_id));
    assert_eq!(moved.authority_epoch, z.authority_epoch);
    assert_eq!(moved.grant.seat_session_id, z_seat);
    assert_eq!(
        chain(&store, b),
        [(0, pinnacle.node_id), (1, global.node_id), (2, z.node_id)]
    );
    assert_eq!(
        chain(&store, c),
        [(0, pinnacle.node_id), (1, global.node_id)]
    );
    for dead in [x_seat, y_seat] {
        assert_eq!(
            code(store.global_seat_grant(dead).unwrap_err()),
            GLOBAL_MANAGER_NOT_SEAT
        );
    }
    let (replayed, _, deduplicated) = store
        .revoke_child_portfolio_node(global_seat, &revoke(&x, "revoke-x"))
        .unwrap();
    assert!(deduplicated);
    assert_eq!(replayed.grant.grant_version, view.grant.grant_version);
    // The sibling and the pinnacle are untouched.
    assert_eq!(
        store
            .get_portfolio_node(sibling.node_id)
            .unwrap()
            .unwrap()
            .state,
        "active"
    );
    assert_eq!(
        store.get_portfolio_node(x2.node_id).unwrap().unwrap().state,
        "active"
    );
}

/// Issue AC5: a node replaces its child's seat. The predecessor's token is
/// refused at once; the successor acts under the child's unchanged ledger
/// principal, so the workers the predecessor launched stay with the node.
/// An operator-granted child's seat and a grandchild's are refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_replaced_child_seat_loses_authority_and_the_node_keeps_its_ledger() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
    );
    let (global, global_seat) = operator_node(&store, "global", &[a, b, c], None, 0);
    let (child, old_seat) = appoint_child(&store, global_seat, "area", &[b, c], 1);
    let (grandchild, _) = appoint_child(&store, old_seat, "team", &[c], 2);
    let (operator_child, _) = operator_node(&store, "ops", &[a], Some(&global), 1);
    let before = principal(&store, old_seat, b).unwrap();
    let mut request = appoint_request(seat_target(child.node_id), "replace-area");
    if let AppointChildTargetV1::Portfolio {
        expected_grant_version,
        ..
    } = &mut request.target
    {
        *expected_grant_version = Some(child.grant.grant_version);
    }
    let (appointment, versions) = appoint(&store, global_seat, &request).unwrap();
    let new_seat = appointment.session_id;
    let replaced = store.get_portfolio_node(child.node_id).unwrap().unwrap();
    assert_eq!(replaced.grant.seat_session_id, new_seat);
    assert_eq!(replaced.authority_epoch, child.authority_epoch);
    assert_eq!(
        versions,
        (replaced.authority_epoch, replaced.grant.grant_version)
    );
    assert_eq!(principal(&store, new_seat, b).unwrap(), before);
    assert_eq!(
        refusal(store.resolve_manager_caller(old_seat, Some(b))),
        "manager_node_custody_changed"
    );
    assert_eq!(
        refusal(store.begin_child_appointment(
            old_seat,
            &appoint_request(child_target("late", &[b], 2), "late")
        )),
        GLOBAL_MANAGER_NOT_SEAT
    );
    // The grandchild the old seat appointed stays: its grantor is the node.
    assert_eq!(
        store
            .get_portfolio_node(grandchild.node_id)
            .unwrap()
            .unwrap()
            .state,
        "active"
    );
    for (target, expected) in [
        (operator_child.node_id, MANAGER_CHILD_OPERATOR_GRANTED),
        (grandchild.node_id, MANAGER_NODE_NOT_IN_SCOPE),
    ] {
        assert_eq!(
            refusal(store.begin_child_appointment(
                global_seat,
                &appoint_request(seat_target(target), &format!("seat-{target}"))
            )),
            expected
        );
    }
    let mut stale = appoint_request(seat_target(child.node_id), "stale-seat");
    if let AppointChildTargetV1::Portfolio {
        expected_grant_version,
        ..
    } = &mut stale.target
    {
        *expected_grant_version = Some(child.grant.grant_version);
    }
    assert_eq!(
        refusal(store.begin_child_appointment(global_seat, &stale)),
        MANAGER_NODE_STALE
    );
}

fn self_handoff(
    node: &PortfolioNodeV1,
    key: &str,
    handoff: &str,
) -> AgentManagerAppointChildRequestV1 {
    let mut request = appoint_request(seat_target(node.node_id), key);
    if let AppointChildTargetV1::Portfolio {
        expected_grant_version,
        ..
    } = &mut request.target
    {
        *expected_grant_version = Some(node.grant.grant_version);
    }
    request.handoff = Some(handoff.into());
    request
}

/// #2021: a node's own seat hands the seat to the session it launches. The
/// node keeps its grant content, epoch and ledger principal; the old seat is
/// refused at once; the handoff (old seat, new seat, path) is recorded and
/// shown on the node's workspace. Operator-granted and node-granted nodes
/// alike.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_portfolio_seat_hands_its_own_seat_on() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
    );
    let (global, old_seat) = operator_node(&store, "global", &[a, b, c], None, 0);
    let (child, child_seat) = appoint_child(&store, old_seat, "area", &[b, c], 1);
    let before = principal(&store, old_seat, a).unwrap();
    assert!(
        store
            .last_seat_handoff_of(global.node_id)
            .unwrap()
            .is_none()
    );

    let request = self_handoff(
        &global,
        "hand-global",
        "thoughts/shared/handoffs/general/g.md",
    );
    let (appointment, versions) = appoint(&store, old_seat, &request).unwrap();
    let new_seat = appointment.session_id;
    let handed = store.get_portfolio_node(global.node_id).unwrap().unwrap();
    assert_eq!(handed.grant.seat_session_id, new_seat);
    assert_eq!(handed.authority_epoch, global.authority_epoch);
    assert_eq!(handed.grant.project_ids, global.grant.project_ids);
    assert_eq!(handed.grant.allowed_launches, global.grant.allowed_launches);
    assert_eq!(handed.grant.project_policy, global.grant.project_policy);
    assert_eq!(handed.grantor, "operator");
    assert_eq!(
        versions,
        (handed.authority_epoch, handed.grant.grant_version)
    );
    assert!(handed.grant.grant_version > global.grant.grant_version);
    assert_eq!(principal(&store, new_seat, a).unwrap(), before);
    assert_eq!(
        refusal(store.resolve_manager_caller(old_seat, Some(a))),
        "manager_node_custody_changed"
    );
    assert_eq!(
        refusal(store.begin_child_appointment(
            old_seat,
            &appoint_request(child_target("late", &[a], 1), "late")
        )),
        GLOBAL_MANAGER_NOT_SEAT
    );
    // The completion the launch published is returned to the old seat's call,
    // not refused as a lost seat.
    assert_eq!(
        store
            .finish_portfolio_appointment(old_seat, &request, &appointment)
            .unwrap(),
        versions
    );
    // The child the old seat appointed stays, under the same grantor node.
    assert_eq!(
        store
            .get_portfolio_node(child.node_id)
            .unwrap()
            .unwrap()
            .grant
            .seat_session_id,
        child_seat
    );
    let shown = store.last_seat_handoff_of(global.node_id).unwrap().unwrap();
    assert_eq!(shown.old_seat, old_seat);
    assert_eq!(shown.new_seat, new_seat);
    assert_eq!(
        shown.handoff.as_deref(),
        Some("thoughts/shared/handoffs/general/g.md")
    );
    // The new seat can hand on again, and the child (granted by the node)
    // hands its own seat on too.
    let again = self_handoff(&handed, "hand-again", "abc1234");
    let (second, _) = appoint(&store, new_seat, &again).unwrap();
    assert_eq!(
        store
            .last_seat_handoff_of(global.node_id)
            .unwrap()
            .unwrap()
            .new_seat,
        second.session_id
    );
    let request = self_handoff(&child, "hand-area", "h.md");
    let (third, _) = appoint(&store, child_seat, &request).unwrap();
    let area = store.get_portfolio_node(child.node_id).unwrap().unwrap();
    assert_eq!(area.grant.seat_session_id, third.session_id);
    assert_eq!(area.grantor, child.grantor);
    assert_eq!(area.parent_node_id, child.parent_node_id);
}

/// #2021: a self-handoff stays inside the node's own allowance and fences on
/// the grant. A launch outside `allowed_launches`, a stale grant version and a
/// handoff reference on any other target are refused before any session or
/// appointment row exists; an operator re-seat between admission and the
/// handoff wins.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_self_handoff_is_fenced_and_stays_inside_the_node() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (global, seat) = operator_node(&store, "global", &[a, b], None, 0);
    let before = (
        count(&store, "sessions"),
        count(&store, "manager_portfolio_appointments"),
    );
    let mut disallowed = self_handoff(&global, "launch", "h.md");
    disallowed.launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Codex,
        model: "gpt-6-astra".into(),
        effort: None,
    };
    let mut stale = self_handoff(&global, "stale", "h.md");
    if let AppointChildTargetV1::Portfolio {
        expected_grant_version,
        ..
    } = &mut stale.target
    {
        *expected_grant_version = Some(global.grant.grant_version + 1);
    }
    let mut outside = self_handoff(&global, "outside", "h.md");
    if let AppointChildTargetV1::Portfolio {
        launch_project_id, ..
    } = &mut outside.target
    {
        *launch_project_id = Some(Uuid::new_v4());
    }
    let mut on_a_pm = appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm");
    on_a_pm.handoff = Some("h.md".into());
    for (request, expected) in [
        (disallowed, GLOBAL_LAUNCH_NOT_ALLOWED),
        (stale, MANAGER_NODE_STALE),
        (outside, MANAGER_PROJECT_NOT_IN_SCOPE),
        (on_a_pm, "portfolio_invalid_request"),
    ] {
        assert_eq!(
            refusal(store.begin_child_appointment(seat, &request)),
            expected
        );
    }
    assert_eq!(
        before,
        (
            count(&store, "sessions"),
            count(&store, "manager_portfolio_appointments")
        )
    );
    // A seat that is not the node's seat has no node to hand on.
    assert_eq!(
        refusal(store.begin_child_appointment(
            session(&store, None),
            &self_handoff(&global, "stranger", "h.md")
        )),
        GLOBAL_MANAGER_NOT_SEAT
    );
    // An operator re-seat between admission and the handoff wins: the
    // handoff is refused and the node stays with the operator's seat.
    let request = self_handoff(&global, "race", "h.md");
    let appointment = store.begin_child_appointment(seat, &request).unwrap();
    launched(
        &store,
        appointment.session_id,
        appointment.launch_project_id,
    );
    let operator_seat = session(&store, None);
    let mut regrant = edit_of(&global, "operator-reseat");
    regrant.seat_session_id = operator_seat;
    let reseated = create(&store, &regrant).unwrap();
    assert_eq!(
        refusal(store.finish_portfolio_appointment(seat, &request, &appointment)),
        GLOBAL_MANAGER_NOT_SEAT
    );
    let node = store.get_portfolio_node(global.node_id).unwrap().unwrap();
    assert_eq!(node.grant.seat_session_id, operator_seat);
    assert_eq!(node.grant.grant_version, reseated.grant.grant_version);
    assert!(
        store
            .last_seat_handoff_of(global.node_id)
            .unwrap()
            .is_none()
    );
}

/// An operator re-grant of the grantor between admission and appointment
/// refuses the appointment; a context-cap successor of the same epoch
/// finishes it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_appointment_is_fenced_on_the_grantor_epoch() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (global, seat) = operator_node(&store, "global", &[a, b], None, 0);
    let request = appoint_request(child_target("area", &[b], 1), "area");
    let appointment = store.begin_child_appointment(seat, &request).unwrap();
    launched(&store, appointment.session_id, b);
    let successor = super::tests::successor_of(&store, seat);
    assert!(store.transfer_global_seat(seat, successor).unwrap());
    let (epoch, _) = store
        .finish_portfolio_appointment(successor, &request, &appointment)
        .unwrap();
    assert!(epoch > 0);
    // A second appointment, then an operator edit of the grantor.
    let request = appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a");
    let appointment = store.begin_child_appointment(successor, &request).unwrap();
    launched(&store, appointment.session_id, a);
    let current = store.get_portfolio_node(global.node_id).unwrap().unwrap();
    create(&store, &super::tests::edit_of(&current, "regrant")).unwrap();
    assert_eq!(
        refusal(store.finish_project_appointment(successor, &request, &appointment)),
        GLOBAL_MANAGER_NOT_SEAT
    );
    assert!(store.get_harness_manager(a).unwrap().is_none());
}

/// Plan §2.4 session control over child seats at depth 3: a seat reads any
/// descendant seat and covered PM seat, halts or continues only a direct
/// child seat, and never reaches an ancestor's or a sibling's seat.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn seats_reach_descendants_read_only_and_direct_children_for_control() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c, d) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
        project(&store, "D"),
    );
    let (pinnacle, p_seat) = operator_node(&store, "pinnacle", &[a, b, c, d], None, 0);
    let (global, g_seat) = operator_node(&store, "global", &[a, b, c], Some(&pinnacle), 1);
    let (_, s_seat) = operator_node(&store, "global", &[d], Some(&pinnacle), 1);
    let (_, x_seat) = appoint_child(&store, g_seat, "area", &[b, c], 2);
    let (_, pm_a) = appoint(
        &store,
        g_seat,
        &appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a"),
    )
    .map(|(appointment, _)| ((), appointment.session_id))
    .unwrap();
    let _ = global;
    let reach = |caller, target, mutation| {
        store
            .portfolio_seat_reach(caller, target, mutation)
            .unwrap()
    };
    // Reads: every descendant seat and covered PM seat.
    for target in [g_seat, x_seat, pm_a, s_seat] {
        assert!(reach(p_seat, target, false), "pinnacle reads {target}");
    }
    assert!(reach(g_seat, x_seat, false));
    assert!(reach(g_seat, pm_a, false));
    // Control: direct children only.
    assert!(reach(p_seat, g_seat, true));
    assert!(reach(g_seat, x_seat, true));
    assert!(reach(g_seat, pm_a, true));
    assert!(!reach(p_seat, x_seat, true), "a grandchild is not direct");
    assert!(!reach(p_seat, pm_a, true), "a grandchild PM is not direct");
    // Never up or sideways.
    for (caller, target) in [
        (g_seat, p_seat),
        (x_seat, g_seat),
        (x_seat, p_seat),
        (g_seat, s_seat),
        (s_seat, g_seat),
        (s_seat, x_seat),
        (pm_a, g_seat),
    ] {
        for mutation in [false, true] {
            assert!(
                !reach(caller, target, mutation),
                "{caller} -> {target} mutation={mutation}"
            );
        }
    }
}

/// Issue AC6 (plan §4 I10): appointment rows are retained, their identity is
/// immutable and an appointed row is final; the catalog rewinds and replays.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn appointment_rows_are_retained_immutable_and_final() {
    let store = Store::open_in_memory().unwrap();
    for (kind, name) in crate::store::portfolio_nodes::delegation::CATALOG_OBJECTS {
        let present: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "{kind} {name}");
    }
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (_, seat) = operator_node(&store, "global", &[a, b], None, 0);
    let (appointment, _) = appoint(
        &store,
        seat,
        &appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a"),
    )
    .unwrap();
    let id = appointment.id.to_string();
    let other = Uuid::new_v4().to_string();
    let attempts = [
        "DELETE FROM manager_portfolio_appointments WHERE id=?1 AND ?2<>''",
        "UPDATE manager_portfolio_appointments SET session_id=?2 WHERE id=?1",
        "UPDATE manager_portfolio_appointments SET grantor_node_id=?2 WHERE id=?1",
        "UPDATE manager_portfolio_appointments SET target_ref='project:'||?2 WHERE id=?1",
        "UPDATE manager_portfolio_appointments SET idempotency_key=?2 WHERE id=?1",
        "UPDATE manager_portfolio_appointments SET state='launched',scope_version=NULL,policy_version=NULL WHERE id=?1 AND ?2<>''",
        "UPDATE manager_portfolio_appointments SET scope_version=scope_version+1 WHERE id=?1 AND ?2<>''",
    ];
    for sql in attempts {
        assert!(
            store.conn.execute(sql, params![id, other]).is_err(),
            "{sql} was allowed"
        );
    }
    assert_eq!(count(&store, "manager_portfolio_appointments"), 1);
    // A wrong target shape is refused by the CHECK.
    let bad = store.conn.execute(
        "INSERT INTO manager_portfolio_appointments(id,grantor_node_id,grantor_authority_epoch,target_ref,launch_project_id,caller_session_id,idempotency_key,request_digest,session_id,state,created_at,updated_at,reserved_at)
         SELECT ?1,grantor_node_id,grantor_authority_epoch,'area:'||launch_project_id,launch_project_id,caller_session_id,'k2',request_digest,?2,'launched',created_at,updated_at,reserved_at
         FROM manager_portfolio_appointments WHERE id=?3",
        params![Uuid::new_v4().to_string(), Uuid::new_v4().to_string(), id],
    );
    assert!(bad.is_err());
    // Rewind below V156 and replay: the catalog comes back.
    // The step below V156 (V155 once S4's M2 is present).
    let previous = super::super::MIGRATION_STEPS
        .iter()
        .map(|&(step, _)| step)
        .filter(|&step| {
            step < crate::store::portfolio_nodes::delegation::PORTFOLIO_APPOINTMENT_SCHEMA_VERSION
        })
        .max()
        .unwrap();
    super::super::tests::rewind_post_v121_tail_to(&store.conn, previous);
    let gone: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='manager_portfolio_appointments'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(gone, 0);
    let version: i32 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    store.migrate_v156(version).unwrap();
    let back: i32 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        back,
        crate::store::portfolio_nodes::delegation::PORTFOLIO_APPOINTMENT_SCHEMA_VERSION
    );
}

/// I1: a node-granted configure can never create a root or adopt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_node_grantor_never_creates_a_root_or_adopts() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let (global, _) = operator_node(&store, "global", &[a], None, 0);
    let seat = session(&store, None);
    let rooted = root("rogue", seat, &[a], "rogue");
    assert_eq!(
        refusal(store.configure_portfolio_node(
            &rooted,
            PortfolioGrantor::Node(global.node_id),
            "agent:test"
        )),
        rsi_common::portfolio_nodes::MANAGER_NODE_ROOT_OPERATOR_ONLY
    );
    let mut adopting = root("rogue", seat, &[a], "rogue-adopt");
    adopting.parent_node_id = Some(global.node_id);
    adopting.adopt_node_ids = vec![Uuid::new_v4()];
    assert_eq!(
        refusal(store.configure_portfolio_node(
            &adopting,
            PortfolioGrantor::Node(global.node_id),
            "agent:test"
        )),
        rsi_common::portfolio_nodes::MANAGER_NODE_ROOT_OPERATOR_ONLY
    );
}

/// #1287: a Claude-only node never hands a child node or a PM wider
/// launches or allowances: the operator's configure, a node-created child
/// and the PM policy are all held to the node's own policy.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_claude_only_node_never_hands_down_wider_launches() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let codex = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Codex,
        model: "gpt-6-astra".into(),
        effort: None,
    };
    let wide = ManagerPolicyV2 {
        allowed_launches: vec![codex.clone()],
        ..tier_policy(1)
    };
    // The operator cannot configure a widening child policy.
    let seat = session(&store, None);
    let mut request = root("global", seat, &[a, b], "wide-child-policy");
    request.policy = tier_policy(0);
    request.child_policy = Some(wide.clone());
    assert_eq!(
        refusal(create(&store, &request)),
        MANAGER_CAPABILITY_WIDENED
    );
    let mut equal = request.clone();
    equal.idempotency_key = "equal-child-policy".into();
    equal.child_policy = Some(tier_policy(0));
    assert_eq!(refusal(create(&store, &equal)), MANAGER_ALLOWANCE_EXCEEDED);
    // A Claude-only node with no child policy.
    let (_, seat) = operator_node(&store, "global", &[a, b], None, 0);
    let before = (
        count(&store, "sessions"),
        count(&store, "manager_portfolio_appointments"),
    );
    let mut target = child_target("area", &[b], 1);
    if let AppointChildTargetV1::Portfolio { child_policy, .. } = &mut target {
        *child_policy = Some(ManagerPolicyV2 {
            allowed_launches: vec![codex],
            ..tier_policy(2)
        });
    }
    assert_eq!(
        refusal(store.begin_child_appointment(seat, &appoint_request(target, "wide-node"))),
        MANAGER_CAPABILITY_WIDENED
    );
    assert_eq!(
        (
            count(&store, "sessions"),
            count(&store, "manager_portfolio_appointments")
        ),
        before,
        "refused before any session"
    );
    // #1412: the PM it appoints inherits the node's launches live (its own
    // list stays empty), and the gate holds it to the node's Claude launch,
    // never "any".
    appoint(
        &store,
        seat,
        &appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a"),
    )
    .unwrap();
    let saved = store.get_harness_manager_policy(a).unwrap().unwrap();
    assert!(saved.policy.allowed_launches.is_empty());
    let pm = store.get_harness_manager(a).unwrap().unwrap();
    store.manager_ancestor_launch_gate(&pm, &launch()).unwrap();
    assert_eq!(
        refusal(store.manager_ancestor_launch_gate(
            &pm,
            &ManagerLaunchChoiceV2 {
                provider: SessionProvider::Codex,
                model: "gpt-6-astra".into(),
                effort: None,
            }
        )),
        "manager_v2_launch_not_granted"
    );
}

/// #1412: a PM appointed under a node resolves its launches live against the
/// node's grant. Widening the grant after the appointment reaches the PM at
/// its next launch; narrowing it takes the launch away again.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_pms_launches_follow_the_grant_after_the_appointment() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (node, seat) = operator_node(&store, "global", &[a, b], None, 0);
    appoint(
        &store,
        seat,
        &appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a"),
    )
    .unwrap();
    let pm = store.get_harness_manager(a).unwrap().unwrap();
    let sonnet = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-sonnet-5-5".into(),
        effort: Some("high".into()),
    };
    assert_eq!(
        refusal(store.manager_ancestor_launch_gate(&pm, &sonnet)),
        "manager_v2_launch_not_granted",
        "a grant holding only the manager model refuses a worker launch"
    );
    let pm_row_launches = |store: &Store| {
        store
            .manager_tree_snapshot()
            .unwrap()
            .rows
            .into_iter()
            .find(|row| row.project_id == Some(a) && row.kind == ManagerTreeKindV1::Project)
            .expect("project A row")
            .launches
    };
    let inspected_launches = |store: &Store| {
        use rsi_common::harness_manager_v2::{
            AgentManagerInspectRequestV2, ManagerInspectSectionV2,
        };
        let inspected = store
            .manager_v2_inspect(
                pm.manager_session_id,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Resources,
                    ..Default::default()
                },
            )
            .unwrap();
        inspected.policy.unwrap().policy.allowed_launches
    };
    assert_eq!(pm_row_launches(&store), vec![launch()]);
    assert_eq!(inspected_launches(&store), vec![launch()]);
    assert_eq!(
        refusal(store.manager_launch_policy_gate(&pm, &[], &sonnet)),
        format!(
            "manager_v2_launch_not_granted: allowed_launches={}",
            serde_json::to_string(&vec![launch()]).unwrap()
        )
    );
    let mut widen = edit_of(&node, "widen-launches");
    widen.allowed_launches = vec![launch(), sonnet.clone()];
    let widened = create(&store, &widen).unwrap();
    assert_eq!(inspected_launches(&store), vec![launch(), sonnet.clone()]);
    store.manager_launch_policy_gate(&pm, &[], &sonnet).unwrap();
    // An explicit local list still narrows the inherited ceiling.
    assert!(
        store
            .manager_launch_policy_gate(&pm, &[launch()], &sonnet)
            .is_err()
    );
    assert_eq!(
        pm_row_launches(&store),
        vec![launch(), sonnet.clone()],
        "the tree row shows the live effective set"
    );
    // The PM was appointed before the widening; its policy was not rewritten.
    let pm = store.get_harness_manager(a).unwrap().unwrap();
    store.manager_ancestor_launch_gate(&pm, &sonnet).unwrap();
    assert_eq!(
        store.manager_effective_launches(&pm, &[]).unwrap(),
        vec![launch(), sonnet.clone()]
    );
    store.manager_ancestor_launch_gate(&pm, &launch()).unwrap();
    let mut narrow = edit_of(&widened, "narrow-launches");
    narrow.allowed_launches = vec![launch()];
    create(&store, &narrow).unwrap();
    assert_eq!(inspected_launches(&store), vec![launch()]);
    assert!(store.manager_launch_policy_gate(&pm, &[], &sonnet).is_err());
    let pm = store.get_harness_manager(a).unwrap().unwrap();
    assert_eq!(
        store
            .manager_effective_launches(&pm, &[launch(), sonnet.clone()])
            .unwrap(),
        vec![launch()],
        "an existing copied PM list is clamped by the live grant"
    );
    assert_eq!(pm_row_launches(&store), vec![launch()]);
    assert_eq!(
        refusal(store.manager_ancestor_launch_gate(&pm, &sonnet)),
        "manager_v2_launch_not_granted"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_inherited_pm_without_ancestors_fails_closed() {
    use rsi_common::portfolio_nodes::RevokePortfolioNodeRequestV1;
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let root_seat = session(&store, Some(a));
    let independent = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: a,
            session_id: root_seat,
            group_ids: vec![],
            epic_ids: None,
            expected_row_version: 0,
        })
        .unwrap();
    store
        .manager_ancestor_launch_gate(&independent, &launch())
        .unwrap();
    assert_eq!(
        store.manager_effective_launches(&independent, &[]).unwrap(),
        vec![]
    );
    assert_eq!(
        refusal(store.manager_launch_policy_gate(&independent, &[], &launch())),
        format!(
            "manager_v2_launch_not_granted: allowed_launches=[]; {}",
            rsi_common::satellite_manager_policy::NO_LAUNCH_CHOICES_HINT
        )
    );
    store
        .manager_launch_policy_gate(&independent, &[launch()], &launch())
        .unwrap();

    let (node, seat) = operator_node(&store, "global", &[a], None, 0);
    appoint(
        &store,
        seat,
        &appoint_request(AppointChildTargetV1::Project { project_id: a }, "inherit"),
    )
    .unwrap();
    let pm = store.get_harness_manager(a).unwrap().unwrap();
    store.manager_ancestor_launch_gate(&pm, &launch()).unwrap();
    store
        .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
            node_id: node.node_id,
            expected_grant_version: node.grant.grant_version,
            expected_authority_epoch: node.authority_epoch,
            idempotency_key: "remove-last-ancestor".into(),
        })
        .unwrap();
    assert_eq!(store.manager_effective_launches(&pm, &[]).unwrap(), vec![]);
    assert_eq!(
        refusal(store.manager_launch_policy_gate(&pm, &[], &launch())),
        format!(
            "manager_v2_launch_not_granted: allowed_launches=[]; {}",
            rsi_common::satellite_manager_policy::NO_LAUNCH_CHOICES_HINT
        )
    );
    assert_eq!(
        store
            .satellite_manager_launches()
            .unwrap()
            .into_iter()
            .find(|row| row.project_id == a)
            .map(|row| row.launches),
        Some(0)
    );
    assert_eq!(
        refusal(store.manager_ancestor_launch_gate(&pm, &launch())),
        "manager_v2_launch_not_granted"
    );
}

/// #1288: an unfinished appointment is authorized again before its replay
/// may launch: after an operator re-grant of the grantor it is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_unfinished_replay_is_authorized_again() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (global, seat) = operator_node(&store, "global", &[a, b], None, 0);
    let request = appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a");
    let first = store.begin_child_appointment(seat, &request).unwrap();
    // A replay of the same unfinished appointment is admitted again.
    let replay = store.begin_child_appointment(seat, &request).unwrap();
    assert_eq!(replay.session_id, first.session_id);
    assert!(replay.appointed.is_none());
    let current = store.get_portfolio_node(global.node_id).unwrap().unwrap();
    let mut narrowed = super::tests::edit_of(&current, "narrow");
    narrowed.project_ids = vec![b];
    create(&store, &narrowed).unwrap();
    assert_eq!(
        refusal(store.begin_child_appointment(seat, &request)),
        GLOBAL_MANAGER_NOT_SEAT
    );
}

/// #1289: a handed policy that does not fit the target project is refused
/// before any launch, and a policy save that fails at the appointment rolls
/// back the seat change: the previous PM stays.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_pm_seat_and_its_policy_change_together_or_not_at_all() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let group_a = super::tests::hosted(&store, a, None, SessionKind::Group);
    let seat = session(&store, None);
    let mut request = root("global", seat, &[a, b], "grouped");
    request.policy = tier_policy(0);
    request.child_policy = Some(ManagerPolicyV2 {
        group_ids: vec![group_a],
        ..tier_policy(1)
    });
    create(&store, &request).unwrap();
    let before = (
        count(&store, "sessions"),
        count(&store, "manager_portfolio_appointments"),
    );
    assert_eq!(
        refusal(store.begin_child_appointment(
            seat,
            &appoint_request(AppointChildTargetV1::Project { project_id: b }, "pm-b")
        )),
        "manager_v2_group_out_of_scope"
    );
    assert_eq!(
        (
            count(&store, "sessions"),
            count(&store, "manager_portfolio_appointments")
        ),
        before
    );
    // Project A fits; the operator's PM is in place.
    let old_pm = session(&store, Some(a));
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: a,
            session_id: old_pm,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .unwrap();
    let request = appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a");
    let appointment = store.begin_child_appointment(seat, &request).unwrap();
    launched(&store, appointment.session_id, a);
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER test_policy_save_fails BEFORE INSERT ON harness_manager_v2_policies
             BEGIN SELECT RAISE(ABORT,'injected policy failure'); END;",
        )
        .unwrap();
    let error = store
        .finish_project_appointment(seat, &request, &appointment)
        .unwrap_err();
    assert!(
        error.to_string().contains("injected policy failure"),
        "{error}"
    );
    let config = store.get_harness_manager(a).unwrap().unwrap();
    assert_eq!(
        config.current_session_id,
        Some(old_pm),
        "the seat change rolled back"
    );
    store
        .conn
        .execute_batch("DROP TRIGGER test_policy_save_fails;")
        .unwrap();
    let versions = store
        .finish_project_appointment(seat, &request, &appointment)
        .unwrap();
    let config = store.get_harness_manager(a).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(appointment.session_id));
    let policy = store.get_harness_manager_policy(a).unwrap().unwrap();
    assert!(!policy.revoked);
    assert_eq!(versions, (policy.scope_version, policy.row_version));
}

/// #1290: launched-but-unappointed appointments reserve their reports, so
/// concurrent appointments cannot pass `max_direct_reports`; a failed launch
/// releases its reservation.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn in_flight_appointments_reserve_direct_reports() {
    let store = Store::open_in_memory().unwrap();
    let projects: Vec<Uuid> = (0..4).map(|n| project(&store, &format!("P{n}"))).collect();
    let seat = session(&store, None);
    let mut request = root("global", seat, &projects, "capped");
    request.policy = tier_policy(0);
    request.max_direct_reports = 2;
    create(&store, &request).unwrap();
    let first = appoint_request(child_target("one", &projects[..1], 1), "one");
    let second = appoint_request(
        AppointChildTargetV1::Project {
            project_id: projects[1],
        },
        "two",
    );
    let one = store.begin_child_appointment(seat, &first).unwrap();
    let two = store.begin_child_appointment(seat, &second).unwrap();
    let third = appoint_request(child_target("three", &projects[2..3], 1), "three");
    assert_eq!(
        refusal(store.begin_child_appointment(seat, &third)),
        MANAGER_DIRECT_REPORT_CAP
    );
    // Both reserved appointments still finish (each excludes itself).
    launched(&store, one.session_id, projects[0]);
    store
        .finish_portfolio_appointment(seat, &first, &one)
        .unwrap();
    launched(&store, two.session_id, projects[1]);
    store
        .update_session_status(two.session_id, SessionStatus::Failed)
        .unwrap();
    // A failed launch releases its reservation.
    store.begin_child_appointment(seat, &third).unwrap();
}

/// #1291: an appointed row must carry both versions.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_appointed_row_needs_both_versions() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (_, seat) = operator_node(&store, "global", &[a, b], None, 0);
    let appointment = store
        .begin_child_appointment(
            seat,
            &appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a"),
        )
        .unwrap();
    for sql in [
        "UPDATE manager_portfolio_appointments SET state='appointed' WHERE id=?1",
        "UPDATE manager_portfolio_appointments SET state='appointed',scope_version=3 WHERE id=?1",
        "UPDATE manager_portfolio_appointments SET state='appointed',policy_version=3 WHERE id=?1",
    ] {
        assert!(
            store
                .conn
                .execute(sql, [appointment.id.to_string()])
                .is_err(),
            "{sql} was allowed"
        );
    }
}

/// An active area delegate under `project`'s legacy root (raw rows: the
/// delegate-free refusal reads only these).
fn area_delegate(store: &Store, project: Uuid) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let (root, child) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, parent, legacy) in [(root, None, Some(project)), (child, Some(root), None)] {
        store
            .conn
            .execute(
                "INSERT INTO manager_nodes(id,parent_node_id,legacy_project_id,seat_root_session_id,state,grant_version,policy_version,authority_epoch,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,'active',1,0,1,?5,?5)",
                params![
                    id.to_string(),
                    parent.map(|p: Uuid| p.to_string()),
                    legacy.map(|p: Uuid| p.to_string()),
                    session(store, Some(project)).to_string(),
                    now
                ],
            )
            .unwrap();
    }
}

/// #1297: a replay runs the full pre-launch preflight again: an area
/// delegate added before the retry refuses it, so it never launches.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_replay_runs_the_area_delegate_preflight() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (_, seat) = operator_node(&store, "global", &[a, b], None, 0);
    let request = appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a");
    let admitted = store.begin_child_appointment(seat, &request).unwrap();
    area_delegate(&store, a);
    let error = store.begin_child_appointment(seat, &request).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_node_root_has_active_delegates"),
        "{error}"
    );
    assert!(store.get_session(admitted.session_id).unwrap().is_none());
}

/// #1298: at the manager-scope limit a fresh PM request for a scope-less
/// project is refused before any launch, and in-flight appointments count.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_manager_scope_limit_is_checked_before_launch() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (_, seat) = operator_node(&store, "global", &[a, b], None, 0);
    // 63 scopes elsewhere leave one slot.
    for n in 0..63 {
        let other = project(&store, &format!("other{n}"));
        let pm = session(&store, Some(other));
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: other,
                session_id: pm,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
    }
    let first = appoint_request(AppointChildTargetV1::Project { project_id: a }, "pm-a");
    store.begin_child_appointment(seat, &first).unwrap();
    let before = (
        count(&store, "sessions"),
        count(&store, "manager_portfolio_appointments"),
    );
    // A's in-flight appointment holds the last slot.
    assert_eq!(
        refusal(store.begin_child_appointment(
            seat,
            &appoint_request(AppointChildTargetV1::Project { project_id: b }, "pm-b")
        )),
        "manager_project_limit_reached"
    );
    assert_eq!(
        (
            count(&store, "sessions"),
            count(&store, "manager_portfolio_appointments")
        ),
        before
    );
    // The holder itself still replays (it excludes its own reservation).
    store.begin_child_appointment(seat, &first).unwrap();
}

/// #1299: a replay renews its reservation under the lock with the cap
/// rechecked. Under a one-report cap, an expired retry and a fresh request
/// never both get through.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_expired_retry_and_a_fresh_request_never_both_launch() {
    let store = Store::open_in_memory().unwrap();
    let projects: Vec<Uuid> = (0..4).map(|n| project(&store, &format!("P{n}"))).collect();
    let seat = session(&store, None);
    let mut request = root("global", seat, &projects, "cap-one");
    request.policy = tier_policy(0);
    request.max_direct_reports = 1;
    create(&store, &request).unwrap();
    let expire = |id: Uuid| {
        let old = (chrono::Utc::now() - chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "UPDATE manager_portfolio_appointments SET reserved_at=?2 WHERE id=?1",
                params![id.to_string(), old],
            )
            .unwrap();
    };
    let retry = appoint_request(child_target("a", &projects[..1], 1), "retry");
    let a = store.begin_child_appointment(seat, &retry).unwrap();
    expire(a.id);
    // The lapsed reservation frees the slot for a fresh request ...
    let fresh = appoint_request(child_target("b", &projects[1..2], 1), "fresh");
    let b = store.begin_child_appointment(seat, &fresh).unwrap();
    // ... and the expired retry may then not launch as well.
    assert_eq!(
        refusal(store.begin_child_appointment(seat, &retry)),
        MANAGER_DIRECT_REPORT_CAP
    );
    // The other order: B's launch failed and its slot lapsed; A's retry
    // renews its reservation, and a new fresh request is refused.
    launched(&store, b.session_id, projects[1]);
    store
        .update_session_status(b.session_id, SessionStatus::Failed)
        .unwrap();
    let renewed = store.begin_child_appointment(seat, &retry).unwrap();
    assert_eq!(renewed.session_id, a.session_id);
    let reserved_at: String = store
        .conn
        .query_row(
            "SELECT reserved_at FROM manager_portfolio_appointments WHERE id=?1",
            [a.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        (chrono::Utc::now() - crate::store::parse_timestamp(&reserved_at).unwrap()).num_seconds()
            < 60,
        "the replay renewed its reservation"
    );
    assert_eq!(
        refusal(store.begin_child_appointment(
            seat,
            &appoint_request(child_target("c", &projects[2..3], 1), "fresh-2")
        )),
        MANAGER_DIRECT_REPORT_CAP
    );
}

// ---- #1314: an appointment passes its grantor's manager resource gates ----

/// An operator-granted node at `level` with `edit` applied to its policy.
fn node_with(
    store: &Store,
    label: &str,
    projects: &[Uuid],
    parent: Option<&PortfolioNodeV1>,
    level: u16,
    edit: impl FnOnce(&mut ManagerPolicyV2),
) -> (PortfolioNodeV1, Uuid) {
    // This policy fixture exercises metered budgets.
    store
        .set_daemon_setting("claude_billing_mode", "metered_api")
        .unwrap();
    store
        .set_daemon_setting("codex_billing_mode", "metered_api")
        .unwrap();
    let seat = session(store, None);
    let mut request = root(label, seat, projects, &format!("op-{label}-{seat}"));
    request.parent_node_id = parent.map(|parent| parent.node_id);
    request.policy = tier_policy(level);
    edit(&mut request.policy);
    (create(store, &request).unwrap(), seat)
}

/// The operator pauses `node` (an edit: a new grant and epoch for it only).
fn pause(store: &Store, node: &PortfolioNodeV1) {
    let mut edit = super::tests::edit_of(node, &format!("pause-{}", node.node_id));
    edit.policy.paused = true;
    create(store, &edit).unwrap();
}

/// One running worker inside a live Epic of `project`: an active session in
/// every portfolio cohort covering the project.
fn busy_worker(store: &Store, project: Uuid) {
    let mut parent = None;
    for (kind, status) in [
        (SessionKind::Group, SessionStatus::Completed),
        (SessionKind::Epic, SessionStatus::Completed),
        (SessionKind::Task, SessionStatus::Running),
    ] {
        let mut row = test_session(Uuid::new_v4(), PathBuf::from("/tmp/portfolio"));
        row.project_id = Some(project);
        row.parent_id = parent;
        row.session_kind = kind;
        row.status = status;
        store.insert_session(&row).unwrap();
        parent = Some(row.id);
    }
}

/// The Model Control admission of an appointment seat's launch, as the
/// daemon's launch funnel makes it immediately before the provider runs.
fn admit_seat(
    store: &Store,
    appointment: &ChildAppointment,
) -> Result<crate::store::StoreAdmissionOutcome> {
    use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelTier};
    let request = crate::model_control::ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::SessionLaunchFresh,
        provider: Some("Claude".into()),
        model: Some(launch().model),
        backend: Some("Claude".into()),
        effort: launch().effort,
        trigger: "delegated appointment".into(),
        owner: InvocationOwner {
            session_id: Some(appointment.session_id),
            project_id: Some(appointment.launch_project_id),
            ..Default::default()
        },
        dedup_key: Some(format!("global.appoint:{}", appointment.session_id)),
        request_fingerprint: Some("sha256:appointment-test".into()),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: None,
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    };
    let origin = store.manager_v2_appointment_origin(
        appointment.session_id,
        Some(appointment.launch_project_id),
    )?;
    store.admit_model_invocation_with_launch_origin(
        Uuid::new_v4(),
        *crate::model_control::registry::entry(request.purpose),
        ModelTier::Standard,
        &request,
        Some(&origin),
    )
}

fn assert_denied(outcome: crate::store::StoreAdmissionOutcome, code: &str) {
    assert!(
        matches!(&outcome, crate::store::StoreAdmissionOutcome::Denied { reason, .. } if reason.contains(code)),
        "{outcome:?}; expected {code}"
    );
}

/// Refused at admission: nothing recorded, nothing launched.
fn assert_refused_before_any_effect(
    store: &Store,
    caller: Uuid,
    request: &AgentManagerAppointChildRequestV1,
    expected: &str,
) {
    let before = (
        count(store, "sessions"),
        count(store, "manager_portfolio_appointments"),
    );
    let actual = refusal(store.begin_child_appointment(caller, request));
    assert!(
        actual == expected || actual.starts_with(&format!("{expected}:")),
        "expected refusal prefix {expected}, got {actual}"
    );
    assert_eq!(
        (
            count(store, "sessions"),
            count(store, "manager_portfolio_appointments")
        ),
        before,
        "a resource refusal records and launches nothing"
    );
}

/// A new-child target at `level` launching in `launch_project`, with `edit`
/// applied to its policy (it must still narrow the grantor's).
fn child_with(
    key: &str,
    projects: &[Uuid],
    level: u16,
    launch_project: Uuid,
    edit: impl FnOnce(&mut ManagerPolicyV2),
) -> AgentManagerAppointChildRequestV1 {
    let mut request = appoint_request(child_target(key, projects, level), key);
    if let AppointChildTargetV1::Portfolio {
        policy: Some(policy),
        launch_project_id,
        ..
    } = &mut request.target
    {
        edit(policy);
        *launch_project_id = Some(launch_project);
    }
    request
}

fn pm(project_id: Uuid, key: &str) -> AgentManagerAppointChildRequestV1 {
    appoint_request(AppointChildTargetV1::Project { project_id }, key)
}

/// The grantor's own policy: paused, at its concurrency cap, or out of
/// lifetime creation allowance refuses a project and a portfolio target
/// before any effect, as its own `create_session` would be.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_appointment_obeys_the_grantors_own_pause_concurrency_and_allowance() {
    let store = Store::open_in_memory().unwrap();
    let ps: Vec<Uuid> = (0..9).map(|n| project(&store, &format!("P{n}"))).collect();

    let (_, paused) = node_with(&store, "paused", &ps[0..3], None, 0, |p| p.paused = true);
    for request in [
        pm(ps[0], "paused-pm"),
        child_with("paused-child", &ps[2..3], 1, ps[2], |_| {}),
    ] {
        assert_refused_before_any_effect(&store, paused, &request, "manager_v2_policy_paused");
    }

    let (_, full) = node_with(&store, "full", &ps[3..6], None, 0, |p| {
        p.max_active_sessions = 2;
    });
    busy_worker(&store, ps[3]);
    busy_worker(&store, ps[3]);
    for request in [
        pm(ps[3], "full-pm"),
        child_with("full-child", &ps[3..4], 1, ps[3], |p| {
            p.max_active_sessions = 1;
        }),
    ] {
        assert_refused_before_any_effect(&store, full, &request, "manager_v2_concurrency_capacity");
    }
    // Another project's cohort is not full.
    appoint(&store, full, &pm(ps[4], "full-pm-e")).unwrap();

    let (_, spent) = node_with(&store, "spent", &ps[6..9], None, 0, |p| {
        p.max_created_sessions = 1;
    });
    // The first seat is charged to its grantor (#1301) ...
    appoint(&store, spent, &pm(ps[6], "spent-pm-g")).unwrap();
    // ... so neither a second PM there nor a child seat launching there fits
    // its allowance; another project's allowance is untouched.
    assert_refused_before_any_effect(
        &store,
        spent,
        &pm(ps[6], "spent-pm-again"),
        "manager_v2_creation_limit",
    );
    let narrow = |p: &mut ManagerPolicyV2| p.max_created_sessions = 0;
    assert_refused_before_any_effect(
        &store,
        spent,
        &child_with("spent-child", &ps[6..7], 1, ps[6], narrow),
        "manager_v2_creation_limit",
    );
    appoint(&store, spent, &pm(ps[7], "spent-pm-h")).unwrap();
}

/// Every ancestor's caps apply too: a paused ancestor, and an ancestor
/// whose allowance in the project its own seats used up, refuse a
/// descendant's appointment of a project or portfolio seat.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_appointment_obeys_every_ancestors_pause_and_allowance() {
    let store = Store::open_in_memory().unwrap();
    let ps: Vec<Uuid> = (0..4).map(|n| project(&store, &format!("P{n}"))).collect();
    let (paused_root, _) = node_with(&store, "root", &ps, None, 0, |p| p.paused = true);
    let (_, mid) = node_with(&store, "mid", &ps[1..], Some(&paused_root), 1, |_| {});
    for request in [
        pm(ps[1], "under-paused-pm"),
        child_with("leaf", &ps[2..3], 2, ps[2], |_| {}),
    ] {
        assert_refused_before_any_effect(&store, mid, &request, "manager_v2_policy_paused");
    }

    let qs: Vec<Uuid> = (0..4).map(|n| project(&store, &format!("Q{n}"))).collect();
    let (top, top_seat) = node_with(&store, "top", &qs, None, 0, |p| {
        p.max_created_sessions = 2;
    });
    let (_, low) = node_with(&store, "low", &qs[2..], Some(&top), 1, |p| {
        p.max_created_sessions = 1;
    });
    // The ancestor's own seats (a PM, then its replacement) use up its
    // allowance in Q2 ...
    appoint(&store, top_seat, &pm(qs[2], "top-pm")).unwrap();
    appoint(&store, top_seat, &pm(qs[2], "top-pm-replacement")).unwrap();
    // ... so the descendant, inside its own allowance, is refused there.
    let narrow = |p: &mut ManagerPolicyV2| p.max_created_sessions = 0;
    for request in [
        pm(qs[2], "low-pm"),
        child_with("low-child", &qs[2..3], 2, qs[2], narrow),
    ] {
        assert_refused_before_any_effect(
            &store,
            low,
            &request,
            rsi_common::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED,
        );
    }
    // Q3 holds none of the ancestor's charges: the child seat fits there.
    appoint(
        &store,
        low,
        &child_with("low-child-q3", &qs[3..], 2, qs[3], narrow),
    )
    .unwrap();
}

/// Admission passed, then a cap changed before the provider ran: the
/// launch's Model Control admission (the effect) refuses it. Project target:
/// an ancestor paused mid-launch. Portfolio target: the grantor's own
/// cohort filled mid-launch. An undrifted launch is admitted and reserves
/// its slot in the grantor's ledger.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_cap_that_changes_between_admission_and_effect_refuses_the_launch() {
    let store = Store::open_in_memory().unwrap();
    let ps: Vec<Uuid> = (0..4).map(|n| project(&store, &format!("P{n}"))).collect();
    let (top, _) = node_with(&store, "top", &ps, None, 0, |_| {});
    let (_, mid) = node_with(&store, "mid", &ps[1..], Some(&top), 1, |p| {
        p.max_active_sessions = 2;
    });

    // Project target, ancestor drift.
    let appointment = store
        .begin_child_appointment(mid, &pm(ps[1], "drift-pm"))
        .unwrap();
    pause(&store, &top);
    assert_denied(
        admit_seat(&store, &appointment).unwrap(),
        "manager_v2_policy_paused",
    );
    assert!(store.get_session(appointment.session_id).unwrap().is_none());
    // The replay is refused at admission now, before any launch.
    assert_eq!(
        refusal(store.begin_child_appointment(mid, &pm(ps[1], "drift-pm"))),
        "manager_v2_policy_paused"
    );

    // Portfolio target, own drift (the paused ancestor covers only P0..P3;
    // a fresh chain keeps the cases apart).
    let qs: Vec<Uuid> = (0..3).map(|n| project(&store, &format!("Q{n}"))).collect();
    let (_, grantor) = node_with(&store, "grantor", &qs, None, 0, |p| {
        p.max_active_sessions = 2;
    });
    let child = child_with("drift-child", &qs[..1], 1, qs[0], |p| {
        p.max_active_sessions = 1;
    });
    let appointment = store.begin_child_appointment(grantor, &child).unwrap();
    busy_worker(&store, qs[0]);
    busy_worker(&store, qs[0]);
    assert_denied(
        admit_seat(&store, &appointment).unwrap(),
        "manager_v2_concurrency_capacity",
    );

    // No drift: admitted, and the in-flight seat holds one of the grantor's
    // slots before its session exists.
    let appointment = store
        .begin_child_appointment(grantor, &pm(qs[1], "steady-pm"))
        .unwrap();
    assert!(matches!(
        admit_seat(&store, &appointment).unwrap(),
        crate::store::StoreAdmissionOutcome::Admitted(_)
    ));
    let head = store
        .portfolio_chain_heads(qs[1])
        .unwrap()
        .into_iter()
        .find(|head| head.seat_root == grantor)
        .unwrap();
    let config = crate::store::manager_resources::portfolio_coverage_config(
        qs[1],
        &head,
        &store.global_project_epics(qs[1]).unwrap(),
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
}

/// The launch funnel always passes the appointment origin: an appointment
/// launch key admitted without one is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_appointment_launch_needs_its_origin() {
    use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelTier};
    let store = Store::open_in_memory().unwrap();
    let session_id = Uuid::new_v4();
    let request = crate::model_control::ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::SessionLaunchFresh,
        provider: Some("Claude".into()),
        model: Some(launch().model),
        backend: Some("Claude".into()),
        effort: None,
        trigger: "delegated appointment".into(),
        owner: InvocationOwner {
            session_id: Some(session_id),
            ..Default::default()
        },
        dedup_key: Some(format!("global.appoint:{session_id}")),
        request_fingerprint: Some("sha256:appointment-test".into()),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: None,
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    };
    let error = store
        .admit_model_invocation_with_launch_origin(
            Uuid::new_v4(),
            *crate::model_control::registry::entry(request.purpose),
            ModelTier::Standard,
            &request,
            None,
        )
        .unwrap_err();
    assert_eq!(code(error), "manager_appointment_origin_required");
    // Nor can an origin be minted for a seat no appointment reserved.
    assert_eq!(
        refusal(
            store
                .manager_v2_appointment_origin(session_id, None)
                .map(|_| ())
        ),
        "manager_appointment_launch_changed"
    );
}

/// One running Codex worker inside a live Epic of `project` with spend
/// `cost`, created by the ledger of the node `seat` seats (a journaled
/// `create_session` naming it as its target), or by no node.
fn node_worker(store: &Store, project: Uuid, seat: Option<Uuid>, cost: f64) -> Uuid {
    // These budget fixtures model a metered Codex login.
    store
        .set_daemon_setting("codex_billing_mode", "metered_api")
        .unwrap();
    let mut parent = None;
    for (kind, status) in [
        (SessionKind::Group, SessionStatus::Completed),
        (SessionKind::Epic, SessionStatus::Completed),
        (SessionKind::Task, SessionStatus::Running),
    ] {
        let mut row = test_session(Uuid::new_v4(), PathBuf::from("/tmp/portfolio"));
        row.project_id = Some(project);
        row.parent_id = parent;
        row.session_kind = kind;
        row.status = status;
        row.provider = SessionProvider::Codex;
        row.cost_usd = Some(if kind == SessionKind::Task { cost } else { 0.0 });
        store.insert_session(&row).unwrap();
        parent = Some(row.id);
    }
    let worker = parent.unwrap();
    if let Some(seat) = seat {
        let config = node_authority(store, seat, project).config;
        store
            .seed_manager_action_for_test(
                &config,
                rsi_common::harness_manager_v2::ManagerActionV2::CreateSession {
                    parent_id: store
                        .get_session(worker)
                        .unwrap()
                        .unwrap()
                        .parent_id
                        .unwrap(),
                    kind: SessionKind::Task,
                    query: "worker".into(),
                    launch: launch(),
                    sandbox_source: None,
                },
                rsi_common::harness_manager_v2::ManagerActionStateV2::Succeeded,
                Some(worker),
            )
            .unwrap();
    }
    worker
}

fn node_authority(
    store: &Store,
    seat: Uuid,
    project: Uuid,
) -> crate::store::harness_manager_v2::ManagerAuthorityV2 {
    let principal = store
        .global_project_principal(seat, project)
        .unwrap()
        .unwrap();
    store
        .global_manager_authority_for(&principal, seat)
        .unwrap()
}

/// The gate of a new Codex `create_session` by the node `seat` seats: its
/// own caps and every ancestor's.
fn node_gate(store: &Store, seat: Uuid, project: Uuid) -> Result<()> {
    let authority = node_authority(store, seat, project);
    store.manager_v2_resource_gate_with(
        &authority.config,
        Some(&authority.grant.policy),
        None,
        SessionProvider::Codex,
        None,
        true,
        &[],
    )
}

/// #1309, plan §2.3(d): a live session counts only against the node that
/// originated it and that node's ancestors. An ancestor's workers never fill
/// a descendant's concurrency cap (nor does the descendant's full cap refuse
/// the ancestor's work), and a descendant's workers still fill the
/// ancestor's.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn live_sessions_charge_their_origin_and_its_ancestors_never_a_descendant() {
    let store = Store::open_in_memory().unwrap();
    let p = project(&store, "P");
    let (top, top_seat) = node_with(&store, "top", &[p], None, 0, |p| {
        p.max_active_sessions = 4;
    });
    let (_, low_seat) = node_with(&store, "low", &[p], Some(&top), 1, |p| {
        p.max_active_sessions = 1;
    });
    let top_worker = node_worker(&store, p, Some(top_seat), 0.0);
    node_worker(&store, p, Some(top_seat), 0.0);
    // The ancestor's two workers leave the descendant's one slot free.
    node_gate(&store, low_seat, p).unwrap();
    // The descendant's worker fills its own cap ...
    let low_worker = node_worker(&store, p, Some(low_seat), 0.0);
    assert!(
        refusal(node_gate(&store, low_seat, p)).starts_with("manager_v2_concurrency_capacity:")
    );
    // ... and is the ancestor's third of four.
    node_gate(&store, top_seat, p).unwrap();
    // The full descendant does not refuse the ancestor's own work; its own
    // continuation still replaces its slot.
    store
        .manager_v2_resource_gate_for_session(top_worker, SessionProvider::Codex)
        .unwrap();
    store
        .manager_v2_resource_gate_for_session(low_worker, SessionProvider::Codex)
        .unwrap();
    // Work no node originated is every node's: it fills the ancestor.
    node_worker(&store, p, None, 0.0);
    assert!(
        refusal(node_gate(&store, top_seat, p)).starts_with("manager_v2_concurrency_capacity:")
    );
}

/// #1309: spend and provider caps follow the same rule as concurrency.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn spend_and_provider_caps_charge_the_origin_and_its_ancestors() {
    use rsi_common::harness_manager_v2::ManagerProviderLimitV2;
    let store = Store::open_in_memory().unwrap();
    let (p, q) = (project(&store, "P"), project(&store, "Q"));
    let codex = |max_active| ManagerProviderLimitV2 {
        provider: SessionProvider::Codex,
        max_active,
    };
    let (top, top_seat) = node_with(&store, "top", &[p, q], None, 0, |p| {
        p.max_spend_usd = Some(10.0);
    });
    let (_, low_seat) = node_with(&store, "low", &[p, q], Some(&top), 1, |p| {
        p.max_spend_usd = Some(1.0);
    });

    // Spend, in P: the ancestor's $5 is not the descendant's.
    node_worker(&store, p, Some(top_seat), 5.0);
    node_gate(&store, low_seat, p).unwrap();
    node_worker(&store, q, Some(low_seat), 2.0);
    assert_eq!(
        refusal(node_gate(&store, low_seat, q)),
        "manager_v2_spend_exhausted"
    );
    node_gate(&store, top_seat, q).unwrap();
    node_worker(&store, q, Some(low_seat), 8.0);
    assert_eq!(
        refusal(node_gate(&store, top_seat, q)),
        "manager_v2_spend_exhausted"
    );

    // Provider, in a fresh project under a fresh chain.
    let r = project(&store, "R");
    let (top, top_seat) = node_with(&store, "top-r", &[r], None, 0, |p| {
        p.provider_limits = vec![codex(3)];
    });
    let (_, low_seat) = node_with(&store, "low-r", &[r], Some(&top), 1, |p| {
        p.provider_limits = vec![codex(1)];
    });
    node_worker(&store, r, Some(top_seat), 0.0);
    node_worker(&store, r, Some(top_seat), 0.0);
    node_gate(&store, low_seat, r).unwrap();
    node_worker(&store, r, Some(low_seat), 0.0);
    assert_eq!(
        refusal(node_gate(&store, low_seat, r)),
        "manager_v2_provider_capacity"
    );
    assert_eq!(
        refusal(node_gate(&store, top_seat, r)),
        "manager_v2_provider_capacity"
    );
}

/// #1309, #1314: a seat node N appoints is charged to N and N's ancestors,
/// never to a node below N.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_appointed_seat_is_charged_to_its_grantor_and_its_ancestors() {
    let store = Store::open_in_memory().unwrap();
    let p = project(&store, "P");
    let (top, top_seat) = node_with(&store, "top", &[p], None, 0, |p| {
        p.max_active_sessions = 5;
    });
    let (mid, mid_seat) = node_with(&store, "mid", &[p], Some(&top), 1, |p| {
        p.max_active_sessions = 3;
    });
    let (_, leaf_seat) = node_with(&store, "leaf", &[p], Some(&mid), 2, |p| {
        p.max_active_sessions = 1;
    });
    node_worker(&store, p, Some(top_seat), 0.0);
    node_worker(&store, p, Some(top_seat), 0.0);
    node_worker(&store, p, Some(mid_seat), 0.0);
    // The mid node appoints a PM for P; its launch holds a slot.
    let appointment = store
        .begin_child_appointment(mid_seat, &pm(p, "mid-pm"))
        .unwrap();
    assert!(matches!(
        admit_seat(&store, &appointment).unwrap(),
        crate::store::StoreAdmissionOutcome::Admitted(_)
    ));
    // The node below the grantor pays for neither the seat nor the work
    // above it: its one slot is free (mid holds 2 of 3, top 4 of 5).
    node_gate(&store, leaf_seat, p).unwrap();
    // One more mid worker: the grantor's worker, worker and seat fill its
    // three slots, and the ancestor's two workers, the mid's two and the
    // seat fill its five.
    node_worker(&store, p, Some(mid_seat), 0.0);
    assert!(
        refusal(node_gate(&store, mid_seat, p)).starts_with("manager_v2_concurrency_capacity:")
    );
    assert!(
        refusal(node_gate(&store, top_seat, p)).starts_with("manager_v2_concurrency_capacity:")
    );
}

/// #1330: a seat a descendant appoints counts against every ancestor's
/// concurrency cap and spend, through the same Model Control admission the
/// launch funnel makes. Root covers [A, B] with two slots; its child covers
/// [A] with one. The child's PM holds the child's slot and one of the root's,
/// so the root's second PM in A is admitted and its third is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_descendants_appointed_seat_fills_its_ancestors_caps() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (root_node, root_seat) = node_with(&store, "root", &[a, b], None, 0, |p| {
        p.max_active_sessions = 2;
    });
    let (_, child_seat) = node_with(&store, "child", &[a], Some(&root_node), 1, |p| {
        p.max_active_sessions = 1;
    });
    let child_pm = store
        .begin_child_appointment(child_seat, &pm(a, "child-pm"))
        .unwrap();
    assert!(matches!(
        admit_seat(&store, &child_pm).unwrap(),
        crate::store::StoreAdmissionOutcome::Admitted(_)
    ));
    let first = store
        .begin_child_appointment(root_seat, &pm(a, "root-pm-1"))
        .unwrap();
    assert!(matches!(
        admit_seat(&store, &first).unwrap(),
        crate::store::StoreAdmissionOutcome::Admitted(_)
    ));
    // The third seat in A does not fit the root's two slots, whether it is
    // refused at the appointment or at the launch's admission.
    match store.begin_child_appointment(root_seat, &pm(a, "root-pm-2")) {
        Ok(second) => assert_denied(
            admit_seat(&store, &second).unwrap(),
            "manager_v2_concurrency_capacity",
        ),
        Err(error) => assert!(code(error).starts_with("manager_v2_concurrency_capacity:")),
    }
    let live: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM model_invocations WHERE project_id=?1
               AND admission_status='admitted'",
            [a.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(live, 2, "only the two seats that fit the root's cap launch");
}

/// #1330: the spend of a seat a descendant appointed is the ancestor's
/// historical spend too, so it exhausts the ancestor's budget.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_descendants_appointed_seat_spends_its_ancestors_budget() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (root_node, root_seat) = node_with(&store, "root", &[a, b], None, 0, |p| {
        p.max_spend_usd = Some(4.0);
    });
    let (_, child_seat) = node_with(&store, "child", &[a], Some(&root_node), 1, |p| {
        p.max_spend_usd = Some(4.0);
    });
    // Before the seat launches, the root may create work in A.
    node_gate(&store, root_seat, a).unwrap();
    let child_pm = store
        .begin_child_appointment(child_seat, &pm(a, "child-pm"))
        .unwrap();
    assert!(matches!(
        admit_seat(&store, &child_pm).unwrap(),
        crate::store::StoreAdmissionOutcome::Admitted(_)
    ));
    let spend = |cost: f64| {
        store
            .conn
            .execute(
                "UPDATE model_invocations SET estimated_cost_usd=?2 WHERE session_id=?1",
                rusqlite::params![child_pm.session_id.to_string(), cost],
            )
            .unwrap();
    };
    // The admitted launch has no price yet: the root sees that unknown spend.
    assert_eq!(
        refusal(node_gate(&store, root_seat, a)),
        "manager_v2_spend_unknown"
    );
    // Priced inside the budget, the root still has room ...
    spend(3.0);
    node_gate(&store, root_seat, a).unwrap();
    // ... and over it, the child's seat exhausts the child and the root.
    spend(5.0);
    for seat in [child_seat, root_seat] {
        assert_eq!(
            refusal(node_gate(&store, seat, a)),
            "manager_v2_spend_exhausted"
        );
    }
}

/// #1336: the resource snapshot reports what the gate charges. After #1309
/// a node's gate counts its own ledger plus the chain ledgers below it (a
/// descendant's appointed seat); the snapshot counted one ledger, so readers
/// saw fewer live sessions and less spend than the gate enforced.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_resource_snapshot_reports_the_gates_charged_usage() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let (root_node, root_seat) = node_with(&store, "root", &[a, b], None, 0, |p| {
        p.max_active_sessions = 4;
    });
    let (_, child_seat) = node_with(&store, "child", &[a], Some(&root_node), 1, |p| {
        p.max_active_sessions = 2;
    });
    for (seat, key) in [(child_seat, "child-pm"), (root_seat, "root-pm")] {
        let appointment = store.begin_child_appointment(seat, &pm(a, key)).unwrap();
        assert!(matches!(
            admit_seat(&store, &appointment).unwrap(),
            crate::store::StoreAdmissionOutcome::Admitted(_)
        ));
    }
    let agrees = |seat: Uuid, active: u64| {
        let config = node_authority(&store, seat, a).config;
        let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
        let (gate_active, gate_claude, gate_known, gate_unknown) = store
            .manager_v2_own_charged_usage_for_test(&config, SessionProvider::Claude)
            .unwrap();
        assert_eq!(gate_active, active, "{snapshot}");
        assert_eq!(snapshot["active_sessions"].as_u64(), Some(gate_active));
        assert_eq!(
            snapshot["active_by_provider"]["Claude"]
                .as_u64()
                .unwrap_or(0),
            gate_claude
        );
        assert_eq!(snapshot["known_spend_usd"].as_f64(), gate_known);
        assert_eq!(
            snapshot["unknown_spend_observations"].as_u64(),
            Some(gate_unknown)
        );
        assert_eq!(snapshot["accounting"], "charged");
        (gate_known, gate_unknown)
    };
    // The root is charged for its own PM and the child's appointed PM; the
    // child only for its own.
    let (_, unknown) = agrees(root_seat, 2);
    assert!(unknown > 0, "the unpriced launches are unknown spend");
    agrees(child_seat, 1);

    store
        .conn
        .execute(
            "UPDATE model_invocations SET estimated_cost_usd=2.5 WHERE project_id=?1",
            [a.to_string()],
        )
        .unwrap();
    assert_eq!(agrees(root_seat, 2), (Some(5.0), 0));
    assert_eq!(agrees(child_seat, 1), (Some(2.5), 0));
}

/// #1905: a wildcard effort reaches PM admission, inspection and live ancestry.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_no_effort_grant_admits_every_pm_effort_and_intersects_exact_choices() {
    let store = Store::open_in_memory().unwrap();
    let project_id = project(&store, "P");
    let (node, seat) = operator_node(&store, "global", &[project_id], None, 0);
    let mut edit = edit_of(&node, "any-effort");
    edit.allowed_launches[0].effort = None;
    let any = edit.allowed_launches[0].clone();
    let node = create(&store, &edit).unwrap();
    appoint(
        &store,
        seat,
        &appoint_request(
            AppointChildTargetV1::Project { project_id },
            "pm-any-effort",
        ),
    )
    .unwrap();
    let pm = store.get_harness_manager(project_id).unwrap().unwrap();
    for effort in rsi_common::model_utils::effort_ladder(&any.model) {
        let exact = ManagerLaunchChoiceV2 {
            effort: Some((*effort).into()),
            ..any.clone()
        };
        Store::global_launch_allowed(&node.grant, &exact).unwrap();
        store.manager_launch_policy_gate(&pm, &[], &exact).unwrap();
        store.manager_ancestor_launch_gate(&pm, &exact).unwrap();
        assert_eq!(
            store
                .manager_effective_launches(&pm, &[exact.clone()])
                .unwrap(),
            vec![exact]
        );
    }
    let other = ManagerLaunchChoiceV2 {
        model: "another-model".into(),
        ..any.clone()
    };
    assert!(Store::global_launch_allowed(&node.grant, &other).is_err());
    assert!(store.manager_launch_policy_gate(&pm, &[], &other).is_err());
    assert!(store.manager_ancestor_launch_gate(&pm, &other).is_err());
    let inspection = store
        .manager_v2_inspect(
            pm.manager_session_id,
            &rsi_common::harness_manager_v2::AgentManagerInspectRequestV2::default(),
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&inspection).unwrap()["launches_display"][0],
        any.display()
    );
    assert_eq!(
        inspection.policy.unwrap().policy.allowed_launches,
        vec![any.clone()]
    );
    let mut exact_edit = edit_of(&node, "exact-effort");
    exact_edit.allowed_launches[0].effort = Some("high".into());
    create(&store, &exact_edit).unwrap();
    let high = exact_edit.allowed_launches[0].clone();
    assert_eq!(
        store
            .manager_effective_launches(&pm, &[any.clone()])
            .unwrap(),
        vec![high.clone()]
    );
    store
        .manager_launch_policy_gate(&pm, &[any.clone()], &high)
        .unwrap();
    let low = ManagerLaunchChoiceV2 {
        effort: Some("low".into()),
        ..any
    };
    assert!(store.manager_launch_policy_gate(&pm, &[], &low).is_err());
    assert!(store.manager_ancestor_launch_gate(&pm, &low).is_err());
}

/// #2494: VM instructions `work` runs on `store`'s connection.
fn vm_ops(store: &Store, work: impl FnOnce()) -> u64 {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    let counter = Arc::new(AtomicU64::new(0));
    let handle = Arc::clone(&counter);
    store.conn.progress_handler(
        1,
        Some(move || {
            handle.fetch_add(1, Ordering::Relaxed);
            false
        }),
    );
    work();
    store.conn.progress_handler(1, None::<fn() -> bool>);
    counter.load(Ordering::Relaxed)
}

/// A project manager with a portfolio node above it and `members` running
/// workers (each with ten admitted invocations, half of them created by the
/// node) under its Epic: the shape of an idle hub whose launches are charged
/// to a node at its cap.
fn busy_project_2494(
    store: &Store,
    members: usize,
) -> (
    rsi_common::harness_manager::HarnessManagerConfigV1,
    rsi_common::types::Session,
) {
    let (config, lead) = crate::store::manager_coordinator::tests::fixture(
        store,
        ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Execute,
            max_active_sessions: 100,
            max_created_sessions: 1000,
            max_created_containers: 60,
            ..ManagerPolicyV2::default()
        },
    );
    let (_node, seat) = node_with(store, "root", &[config.project_id], None, 0, |p| {
        p.max_active_sessions = 100;
    });
    let node_config = node_authority(store, seat, config.project_id).config;
    for n in 0..members {
        let mut child = lead.clone();
        child.id = Uuid::new_v4();
        child.session_kind = SessionKind::Task;
        child.status = SessionStatus::Running;
        child.cost_usd = Some(0.25);
        child.lead_session_id = None;
        store.insert_session(&child).unwrap();
        for _ in 0..10 {
            crate::store::manager_resources::tests::admitted(store, &child);
        }
        if n % 2 == 0 {
            store
                .seed_manager_action_for_test(
                    &node_config,
                    rsi_common::harness_manager_v2::ManagerActionV2::CreateSession {
                        parent_id: lead.parent_id.unwrap(),
                        kind: SessionKind::Task,
                        query: "worker".into(),
                        launch: launch(),
                        sandbox_source: None,
                    },
                    rsi_common::harness_manager_v2::ManagerActionStateV2::Succeeded,
                    Some(child.id),
                )
                .unwrap();
        }
    }
    (config, lead)
}

/// #2494: what one coordinator pass reads per managed project grows with the
/// members it manages at a bounded rate. Counted in SQLite VM instructions
/// (deterministic), as the extra instructions per added member. Before the
/// pass shared one cohort read and skipped members with nothing to refresh, a
/// reconcile cost about 4,400 per member, the question refresh alone 2,200.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_manager_pass_costs_a_bounded_number_of_instructions_per_member_2494() {
    const MEMBERS: u64 = 40;
    let mut cost = Vec::new();
    for members in [0, MEMBERS as usize] {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = busy_project_2494(&store, members);
        let project = config.project_id;
        let snapshot = vm_ops(&store, || {
            store.manager_v2_resource_snapshot(&config).unwrap();
        });
        let questions = vm_ops(&store, || {
            store
                .manager_v2_refresh_question_decisions(&config)
                .unwrap();
        });
        let gate = vm_ops(&store, || {
            store
                .manager_v2_own_charged_usage_for_test(&config, SessionProvider::Codex)
                .unwrap();
        });
        // The first pass writes the projections; the second still reads
        // everything (the idle memo needs a quiet third).
        store.reconcile_manager_intent(project, true).unwrap();
        let reconcile = vm_ops(&store, || {
            store.reconcile_manager_intent(project, true).unwrap();
        });
        cost.push((snapshot, questions, gate, reconcile));
    }
    let per_member = |pick: fn(&(u64, u64, u64, u64)) -> u64| {
        pick(&cost[1]).saturating_sub(pick(&cost[0])) / MEMBERS
    };
    let (snapshot, questions, gate, reconcile) = (
        per_member(|c| c.0),
        per_member(|c| c.1),
        per_member(|c| c.2),
        per_member(|c| c.3),
    );
    eprintln!(
        "issue2494_cost per member: snapshot={snapshot} question_refresh={questions} charged_usage={gate} reconcile_pass={reconcile}; base: {:?}",
        cost[0]
    );
    assert!(snapshot <= 2_000, "snapshot {snapshot}/member");
    assert!(questions <= 800, "question refresh {questions}/member");
    assert!(gate <= 2_000, "charged usage {gate}/member");
    assert!(reconcile <= 2_400, "reconcile pass {reconcile}/member");
}

/// #2494: skipping members with nothing to refresh must not skip a member that
/// has something: a pending question still gets its decision, and the
/// projection of one that is gone still resolves.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_pass_still_projects_the_one_member_holding_a_question_2494() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = busy_project_2494(&store, 12);
    crate::store::manager_coordinator::tests::raise_question(&store, lead.id, 1);
    assert_eq!(
        store
            .manager_v2_refresh_question_decisions(&config)
            .unwrap(),
        1
    );
    let key = format!("question:{}", lead.id);
    let record = store
        .manager_v2_record(&config, "decision", &key)
        .unwrap()
        .unwrap();
    assert_eq!(record.payload["status"], "pending");
    // Only the questioned member has a question projection.
    let projected: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM harness_manager_v2_records
             WHERE project_id=?1 AND record_key LIKE 'question:%' AND kind IN ('decision','intent')",
            [config.project_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(projected, 1);
    // The question goes away: its projection resolves although no member holds it.
    store
        .update_session_pending_question_json(lead.id, None)
        .unwrap();
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let record = store
        .manager_v2_record(&config, "decision", &key)
        .unwrap()
        .unwrap();
    assert_eq!(record.payload["status"], "resolved_externally");
}

/// #2494: the tag and summary hydration behind every session list prepares
/// one statement for any batch size and still returns each session's tags.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn session_list_hydration_uses_one_cached_statement_for_any_batch_2494() {
    let store = Store::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for _ in 0..300 {
        let row = test_session(Uuid::new_v4(), PathBuf::from("/tmp/portfolio"));
        store.insert_session(&row).unwrap();
        ids.push(row.id);
    }
    for (n, tag) in [(3usize, "beta"), (3, "alpha"), (250, "gamma")] {
        store
            .conn
            .execute(
                "INSERT INTO session_tags(session_id,tag) VALUES(?1,?2)",
                params![ids[n].to_string(), tag],
            )
            .unwrap();
    }
    let sessions = store.load_sessions().unwrap();
    assert_eq!(sessions.len(), 300);
    let tags = |id: Uuid| sessions.iter().find(|s| s.id == id).unwrap().tags.clone();
    assert_eq!(tags(ids[3]), vec!["alpha".to_string(), "beta".to_string()]);
    assert_eq!(tags(ids[250]), vec!["gamma".to_string()]);
    assert!(tags(ids[0]).is_empty());
    assert_eq!(
        sessions.iter().find(|s| s.id == ids[3]).unwrap().tag,
        "alpha"
    );
    // A second load reuses the prepared statements: the parse of a 300-way
    // placeholder list is gone, so it costs a bounded number of instructions
    // per session (about 190 measured at the time of the change).
    let ops = vm_ops(&store, || {
        store.load_sessions().unwrap();
    });
    eprintln!("issue2494_cost load_sessions 300 sessions VM ops={ops}");
    assert!(ops <= 300 * 400, "load_sessions {ops} ops");
}

/// #2494: seed `held` queued creates under the project manager.
fn queue_creates_2494(
    store: &Store,
    config: &rsi_common::harness_manager::HarnessManagerConfigV1,
    parent: Uuid,
    held: usize,
) {
    for n in 0..held {
        store
            .seed_manager_action_with_context_for_test(
                config,
                rsi_common::harness_manager_v2::ManagerActionV2::CreateSession {
                    parent_id: parent,
                    kind: SessionKind::Task,
                    query: format!("held {n}"),
                    launch: launch(),
                    sandbox_source: None,
                },
                rsi_common::harness_manager_v2::ManagerActionStateV2::Queued,
                None,
                config.manager_session_id,
            )
            .unwrap();
    }
}

/// #2494: a backlog of creates the host-admission gate holds is looked at on
/// every tick of the manager action loop. The first tick scans the queue (about
/// 1,600 VM instructions per held create); while nothing is written, every
/// later tick only asks the gate about the oldest create again, whatever the
/// backlog, and the gate is still asked once per tick.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_held_create_backlog_costs_a_constant_number_of_instructions_per_tick_2494() {
    const HELD: u64 = 64;
    const TICKS: u64 = 10;
    let mut cost = Vec::new();
    for held in [1, HELD as usize] {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = busy_project_2494(&store, 0);
        queue_creates_2494(&store, &config, lead.parent_id.unwrap(), held);
        let boot = Uuid::new_v4();
        let changes = store.conn.total_changes();
        let asked = std::cell::Cell::new(0u64);
        let tick = || {
            let claim = store
                .claim_manager_action_with_create_admission(boot, false, &|_, _, _| {
                    asked.set(asked.get() + 1);
                    false
                })
                .unwrap();
            assert!(claim.is_none(), "a held create must stay queued");
        };
        let first = vm_ops(&store, tick);
        let reuses = store.manager_intent_idle.borrow().held_claim_reuses;
        let later = vm_ops(&store, || {
            for _ in 0..TICKS {
                tick();
            }
        });
        assert_eq!(
            store.manager_intent_idle.borrow().held_claim_reuses - reuses,
            TICKS,
            "every quiet tick reuses the held claim"
        );
        assert_eq!(store.conn.total_changes(), changes, "a held tick writes");
        // The gate is asked about the oldest create on every tick.
        assert_eq!(asked.get(), TICKS + 1);
        eprintln!(
            "issue2494_cost held creates={held}: first tick={first} ops, quiet tick={} ops",
            later / TICKS
        );
        cost.push((first, later / TICKS));
    }
    assert!(cost[1].0 <= 3_000 * HELD, "scan {} ops", cost[1].0);
    assert!(cost[1].1 <= 400, "a quiet tick costs {} ops", cost[1].1);
    assert!(
        cost[1].1 <= cost[0].1 + 50,
        "quiet tick grows with the backlog: {} -> {}",
        cost[0].1,
        cost[1].1
    );
}

/// #2494: the memo never outlives what it observed: a write, an admission that
/// lets the create go, or a claim of the create itself ends it, and the gate is
/// asked exactly once for a create it admits.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_held_claim_is_reconsidered_after_a_write_or_an_admission_2494() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = busy_project_2494(&store, 0);
    queue_creates_2494(&store, &config, lead.parent_id.unwrap(), 3);
    let boot = Uuid::new_v4();
    let asked = std::cell::Cell::new(0u64);
    let hold = |_: Uuid, _: chrono::DateTime<chrono::Utc>, _: Option<Uuid>| {
        asked.set(asked.get() + 1);
        false
    };
    for _ in 0..3 {
        assert!(
            store
                .claim_manager_action_with_create_admission(boot, false, &hold)
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(store.manager_intent_idle.borrow().held_claim_reuses, 2);
    // A later write (here: another queued create) is rescanned, not reused.
    queue_creates_2494(&store, &config, lead.parent_id.unwrap(), 1);
    assert!(
        store
            .claim_manager_action_with_create_admission(boot, false, &hold)
            .unwrap()
            .is_none()
    );
    assert_eq!(store.manager_intent_idle.borrow().held_claim_reuses, 2);
    // The gate lets the oldest create go: it is claimed after one question.
    let before = asked.get();
    let admit_once = |_: Uuid, _: chrono::DateTime<chrono::Utc>, _: Option<Uuid>| {
        asked.set(asked.get() + 1);
        true
    };
    let claim = store
        .claim_manager_action_with_create_admission(boot, false, &admit_once)
        .unwrap()
        .expect("an admitted create is claimed");
    assert_eq!(asked.get() - before, 1, "the gate is asked once, not twice");
    assert!(claim.operation.is_new_worker_create());
    // The claimed create leaves nothing to reuse.
    assert!(
        store
            .manager_intent_idle
            .borrow()
            .held_claim(
                store.manager_intent_observation_version().unwrap().unwrap(),
                false,
                chrono::Utc::now()
            )
            .is_none()
    );
}
