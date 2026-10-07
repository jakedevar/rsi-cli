//! #1237 (hierarchy S3): operator-created levels above global, through the
//! daemon's own agent entry points. A pinnacle adopts globals and holds the
//! PM verb set over their union; budgets are charged across every ancestor;
//! a grantor-scoped revoke re-roots the operator's globals with their
//! ledgers and workers intact.
#![cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]

use super::*;
use rsi_common::portfolio_nodes::{ConfigurePortfolioNodeRequestV1, PortfolioNodeV1};

/// `policy` with every finite allowance shifted by `delta` (a parent is
/// strictly wider than its child in each one).
fn shifted(policy: &ManagerPolicyV2, delta: i32) -> ManagerPolicyV2 {
    let shift = |value: u16| u16::try_from(i32::from(value) + delta).unwrap();
    ManagerPolicyV2 {
        max_created_containers: shift(policy.max_created_containers),
        max_created_sessions: shift(policy.max_created_sessions),
        max_active_sessions: shift(policy.max_active_sessions),
        ..policy.clone()
    }
}

/// A live leaf seat under `epic` in `project`.
async fn seat_in(p: &Pilot, project: Uuid, epic: Uuid) -> Uuid {
    let seat = Uuid::new_v4();
    live_leaf(p, seat, project, epic).await;
    seat
}

#[allow(clippy::too_many_arguments)]
fn node_request(
    p: &Pilot,
    label: &str,
    seat: Uuid,
    projects: Vec<Uuid>,
    parent: Option<Uuid>,
    adopt: Vec<Uuid>,
    policy: ManagerPolicyV2,
    key: &str,
) -> ConfigurePortfolioNodeRequestV1 {
    ConfigurePortfolioNodeRequestV1 {
        node_id: None,
        parent_node_id: parent,
        adopt_node_ids: adopt,
        expected_parent_grant_version: None,
        tier_label: label.into(),
        seat_session_id: seat,
        project_ids: projects,
        allowed_launches: p.policy.allowed_launches.clone(),
        policy,
        child_policy: None,
        max_direct_reports: 5,
        expected_node_grant_version: 0,
        expected_authority_epoch: 0,
        idempotency_key: key.into(),
    }
}

/// The operator's `ConfigurePortfolioNode` (the RPC handler's store call).
async fn operator_configure(
    p: &Pilot,
    request: &ConfigurePortfolioNodeRequestV1,
) -> PortfolioNodeV1 {
    p.manager
        .store
        .lock()
        .await
        // The operator has reviewed the cap preview (#1398); these tests
        // exercise budget charging, not the confirmation step.
        .configure_portfolio_node_confirmed(
            request,
            crate::store::portfolio_nodes::PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .unwrap()
}

async fn current(p: &Pilot, node: Uuid) -> PortfolioNodeV1 {
    p.manager
        .store
        .lock()
        .await
        .get_portfolio_node(node)
        .unwrap()
        .unwrap()
}

/// A control request fenced on `node`'s current epoch and grant version.
fn node_control(
    node: &PortfolioNodeV1,
    project: Uuid,
    key: &str,
    operation: ManagerActionV2,
) -> AgentManagerControlRequestV2 {
    AgentManagerControlRequestV2 {
        project_id: Some(project),
        fence: ManagerFenceV2 {
            scope_version: node.authority_epoch,
            policy_version: node.grant.grant_version,
        },
        idempotency_key: key.into(),
        operation,
    }
}

/// The S1 portfolio (global over [A, B]) plus a second global over [C], both
/// adopted by a new pinnacle over [A, B, C].
struct Pinnacle {
    g: Portfolio,
    global: PortfolioNodeV1,
    second_seat: Uuid,
    second: PortfolioNodeV1,
    seat: Uuid,
    pinnacle: PortfolioNodeV1,
}

async fn pinnacle_over_two_globals() -> Pinnacle {
    let g = portfolio().await;
    let p = &g.p;
    let first = p
        .manager
        .store
        .lock()
        .await
        .portfolio_node_for_seat(g.seat)
        .unwrap()
        .unwrap();
    let second_seat = seat_in(p, p.project, p.epic).await;
    let second = operator_configure(
        p,
        &node_request(
            p,
            "global",
            second_seat,
            vec![g.c],
            None,
            Vec::new(),
            global_policy(p, ManagerOperatingModeV2::Execute),
            "second-global",
        ),
    )
    .await;
    let seat = seat_in(p, p.project, p.epic).await;
    let pinnacle = operator_configure(
        p,
        &node_request(
            p,
            "pinnacle",
            seat,
            vec![p.project, g.b, g.c],
            None,
            vec![first.node_id, second.node_id],
            // #1302: the two adopted globals' allowances sum below it.
            shifted(&global_policy(p, ManagerOperatingModeV2::Execute), 13),
            "pinnacle",
        ),
    )
    .await;
    let global = current(p, first.node_id).await;
    let second = current(p, second.node_id).await;
    Pinnacle {
        g,
        global,
        second_seat,
        second,
        seat,
        pinnacle,
    }
}

/// Acceptance: a pinnacle that adopts two globals launches an Issue worker in
/// any of their projects; each global still works and is refused outside its
/// own coverage.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pinnacle_over_two_globals_launches_in_any_of_their_projects() {
    let t = pinnacle_over_two_globals().await;
    let (g, p) = (&t.g, &t.g.p);
    assert_eq!(t.global.parent_node_id, Some(t.pinnacle.node_id));
    assert_eq!(t.second.parent_node_id, Some(t.pinnacle.node_id));
    let control = p.manager.agent_control();
    for (project, epic, title) in [(g.b, g.b_epic, "B"), (g.c, g.c_epic, "C")] {
        let issue = new_issue(p, project, title).await;
        let launched = control
            .agent_manager_launch_issue_worker(
                t.seat,
                launch_request(p, Some(project), epic, issue.display_number, title),
            )
            .await
            .unwrap();
        assert!(!launched.deduplicated);
        // The ledger principal is (project, the pinnacle's seat, its epoch).
        let principal: (String, String, i64) = p
            .manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT project_id,manager_session_id,scope_version FROM harness_manager_v2_operations WHERE id=?1",
                [launched.action.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            principal,
            (
                project.to_string(),
                t.seat.to_string(),
                t.pinnacle.authority_epoch
            )
        );
    }
    // Each global still launches in its own coverage after the move.
    for (seat, project, epic, key) in [
        (g.seat, g.b, g.b_epic, "g1-b"),
        (t.second_seat, g.c, g.c_epic, "g2-c"),
    ] {
        let issue = new_issue(p, project, key).await;
        control
            .agent_manager_launch_issue_worker(
                seat,
                launch_request(p, Some(project), epic, issue.display_number, key),
            )
            .await
            .unwrap();
    }
    // ... and is refused outside it.
    for (seat, project, epic, key) in [
        (g.seat, g.c, g.c_epic, "g1-c"),
        (t.second_seat, g.b, g.b_epic, "g2-b"),
    ] {
        let issue = new_issue(p, project, key).await;
        let error = control
            .agent_manager_launch_issue_worker(
                seat,
                launch_request(p, Some(project), epic, issue.display_number, key),
            )
            .await
            .unwrap_err();
        assert!(
            code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
            "{error}"
        );
    }
    // Mutations flow down at depth: the global never mutates the pinnacle's
    // worker; the pinnacle mutates the global's.
    let by_pinnacle = control
        .agent_manager_control(
            t.seat,
            node_control(&t.pinnacle, g.b, "p-b", create_session(p, g.b_epic)),
        )
        .await
        .unwrap()
        .target_session_id
        .unwrap();
    let by_global = control
        .agent_manager_control(
            g.seat,
            node_control(&t.global, g.b, "g-b", create_session(p, g.b_epic)),
        )
        .await
        .unwrap()
        .target_session_id
        .unwrap();
    live_leaf(p, by_pinnacle, g.b, g.b_epic).await;
    live_leaf(p, by_global, g.b, g.b_epic).await;
    let store = p.manager.store.lock().await;
    let error = store
        .manager_session_control_scope(g.seat, by_pinnacle, true)
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_TARGET_OWNED_BY_ANCESTOR),
        "{error}"
    );
    let scope = store
        .manager_session_control_scope(t.seat, by_global, true)
        .unwrap()
        .expect("the pinnacle reaches its descendant's worker");
    assert_eq!(scope.config.manager_session_id, t.seat);
    // Neither reaches the other's seat (no sibling or ancestor seat reach).
    assert!(
        store
            .manager_session_control_scope(g.seat, t.seat, false)
            .unwrap()
            .is_none()
    );
}

/// I9 across three ancestors: a project manager's launch is refused
/// `manager_ancestor_allowance_exceeded` when any one ancestor (swarm,
/// pinnacle or global) has its covered total at its cap, even while the
/// other two have room; and a descendant's cap never limits an ancestor.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn budgets_are_charged_across_three_ancestors() {
    for full in 0..3usize {
        let p = pilot().await;
        let base = global_policy(&p, ManagerOperatingModeV2::Execute);
        // Session caps 3 > 2 > 1 down the chain.
        let policies: Vec<ManagerPolicyV2> = (0..3)
            .map(|depth| {
                let mut policy = shifted(&base, -(depth as i32));
                policy.max_created_sessions = 3 - depth as u16;
                policy
            })
            .collect();
        let mut nodes: Vec<PortfolioNodeV1> = Vec::new();
        let mut seats = Vec::new();
        for depth in 0..3usize {
            let seat = seat_in(&p, p.project, p.epic).await;
            let node = operator_configure(
                &p,
                &node_request(
                    &p,
                    ["swarm", "pinnacle", "global"][depth],
                    seat,
                    vec![p.project],
                    nodes.last().map(|parent| parent.node_id),
                    Vec::new(),
                    policies[depth].clone(),
                    &format!("level-{full}-{depth}"),
                ),
            )
            .await;
            seats.push(seat);
            nodes.push(node);
            // The full node fills its cap before the levels below exist, so
            // only its own covered total is at the cap.
            if depth == full {
                for index in 0..policies[depth].max_created_sessions {
                    p.manager
                        .agent_control()
                        .agent_manager_control(
                            seat,
                            node_control(
                                &nodes[depth],
                                p.project,
                                &format!("fill-{index}"),
                                create_session(&p, p.epic),
                            ),
                        )
                        .await
                        .unwrap();
                }
            }
        }
        let error = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, p.request("pm", create_session(&p, p.epic)))
            .await
            .unwrap_err();
        assert!(
            code(&error).contains(MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED),
            "ancestor {full}: {error}"
        );
        if full == 2 {
            // The swarm's own launch is not limited by the full global below.
            p.manager
                .agent_control()
                .agent_manager_control(
                    seats[0],
                    node_control(&nodes[0], p.project, "swarm", create_session(&p, p.epic)),
                )
                .await
                .unwrap();
        }
    }
    // With room at every level the same launch is admitted.
    let p = pilot().await;
    let base = global_policy(&p, ManagerOperatingModeV2::Execute);
    let mut parent = None;
    for depth in 0..3i32 {
        let seat = seat_in(&p, p.project, p.epic).await;
        let node = operator_configure(
            &p,
            &node_request(
                &p,
                "tier",
                seat,
                vec![p.project],
                parent,
                Vec::new(),
                shifted(&base, -depth),
                &format!("room-{depth}"),
            ),
        )
        .await;
        parent = Some(node.node_id);
    }
    p.manager
        .agent_control()
        .agent_manager_control(p.owner, p.request("pm", create_session(&p, p.epic)))
        .await
        .unwrap();
}

/// Revoke acceptance: revoking the pinnacle re-roots the operator-granted
/// globals with their seats, ledgers and workers intact; the pinnacle's
/// prepared action is refused at commit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_the_pinnacle_reroots_the_globals_with_their_workers() {
    let t = pinnacle_over_two_globals().await;
    let (g, p) = (&t.g, &t.g.p);
    let control = p.manager.agent_control();
    let worker = control
        .agent_manager_control(
            g.seat,
            node_control(&t.global, g.b, "g-worker", create_session(p, g.b_epic)),
        )
        .await
        .unwrap()
        .target_session_id
        .unwrap();
    live_leaf(p, worker, g.b, g.b_epic).await;
    let prepared = control
        .agent_manager_prepare_control(
            t.seat,
            AgentManagerPrepareControlRequestV2 {
                project_id: Some(g.b),
                operation: PreparedManagerActionV2::CreateSession {
                    parent_id: g.b_epic,
                    kind: SessionKind::Task,
                    query: "prepared by the pinnacle".into(),
                    launch: p.policy.allowed_launches[0].clone(),
                    sandbox_source: None,
                },
            },
        )
        .await
        .unwrap();
    let (revoked, outcome) = p
        .manager
        .store
        .lock()
        .await
        .revoke_portfolio_node_outcome(&rsi_common::portfolio_nodes::RevokePortfolioNodeRequestV1 {
            node_id: t.pinnacle.node_id,
            expected_grant_version: t.pinnacle.grant.grant_version,
            expected_authority_epoch: t.pinnacle.authority_epoch,
            idempotency_key: "revoke-pinnacle".into(),
        })
        .unwrap();
    assert_eq!(revoked.state, "revoked");
    assert_eq!(outcome.revoked, [t.pinnacle.node_id]);
    for before in [&t.global, &t.second] {
        let after = current(p, before.node_id).await;
        assert_eq!(after.state, "active");
        assert_eq!(after.parent_node_id, None);
        assert_eq!(after.authority_epoch, before.authority_epoch);
        assert_eq!(after.grant.seat_session_id, before.grant.seat_session_id);
    }
    // The global still controls its worker under the same ledger principal.
    let rerooted = current(p, t.global.node_id).await;
    let scope = p
        .manager
        .store
        .lock()
        .await
        .manager_session_control_scope(g.seat, worker, true)
        .unwrap()
        .expect("the re-rooted global keeps its worker");
    assert_eq!(scope.config.manager_session_id, g.seat);
    assert_eq!(scope.config.row_version, t.global.authority_epoch);
    control
        .agent_manager_control(
            g.seat,
            node_control(&rerooted, g.b, "g-after", create_session(p, g.b_epic)),
        )
        .await
        .unwrap();
    // The revoked pinnacle's stale action is refused at commit.
    let error = control
        .agent_manager_commit_prepared_control(
            t.seat,
            AgentManagerCommitPreparedControlRequestV2 {
                project_id: Some(g.b),
                prepared_id: prepared.prepared_id,
                target_digest: prepared.target_digest.clone(),
                idempotency_key: "commit".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
        "{error}"
    );
}
