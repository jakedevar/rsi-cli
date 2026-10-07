//! #1238 (fractal manager hierarchy S4): N-level reports, escalations and
//! down-mail, pinned on a chain of depth five:
//! leaf area -> parent area -> project root (PM) -> global -> pinnacle ->
//! operator, built with the operator's portfolio RPCs (S3 nests nodes).

use super::*;
use crate::store::manager_nodes::tests::{area_fixture, area_request};
use crate::store::manager_nodes::{AreaNode, RevokeAreaNode};
use crate::test_support::test_session;
use rsi_common::global_manager::GLOBAL_MANAGER_NOT_SEAT;
use rsi_common::harness_manager::{
    AgentManagerEscalateRequestV1, AgentManagerInboxRequestV1,
    AgentManagerResolveEscalationRequestV1, ManagerNodeEscalationRouteV1,
    ManagerNodeEscalationStateV1,
};
use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::manager_nodes::ManagerNodeSelectorV1;
use rsi_common::portfolio_nodes::{ConfigurePortfolioNodeRequestV1, RevokePortfolioNodeRequestV1};
use rsi_common::types::{Approval, ApprovalStatus, Project, SessionProvider};
use std::path::PathBuf;

use crate::store::portfolio_nodes::PortfolioGrantor;

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

fn seat(store: &Store, project: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/tier-routing"));
    row.project_id = project;
    row.status = SessionStatus::Completed;
    store.insert_session(&row).unwrap();
    id
}

fn project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    store
        .insert_project(&Project {
            id,
            name: name.into(),
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

fn root_node(store: &Store, label: &str, seat: Uuid, projects: &[Uuid]) -> Uuid {
    // The chain fixture lowers the area's 100-session cap to the root's four.
    store
        .configure_portfolio_node_confirmed(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: None,
                adopt_node_ids: vec![],
                expected_parent_grant_version: None,
                tier_label: label.into(),
                seat_session_id: seat,
                project_ids: projects.to_vec(),
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: None,
                }],
                policy: ManagerPolicyV2::default(),
                child_policy: None,
                max_direct_reports: 5,
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: format!("root-{label}-{seat}"),
            },
            PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .unwrap()
        .node_id
}

/// A child node under `parent` covering `projects` one level deeper, through
/// the operator's `ConfigurePortfolioNode {parent_node_id}` (#1237).
fn nest(store: &Store, parent: Uuid, label: &str, seat: Uuid, projects: &[Uuid]) -> Uuid {
    let parent_view = store.get_portfolio_node(parent).unwrap().unwrap();
    // Grants narrow going down: every finite allowance is strictly lower.
    let mut policy = parent_view.grant.project_policy.clone();
    policy.max_active_sessions = policy.max_active_sessions.saturating_sub(1);
    policy.max_created_sessions = policy.max_created_sessions.saturating_sub(1);
    policy.max_created_containers = policy.max_created_containers.saturating_sub(1);
    store
        .configure_portfolio_node_confirmed(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: Some(parent),
                adopt_node_ids: vec![],
                expected_parent_grant_version: Some(parent_view.grant.grant_version),
                tier_label: label.into(),
                seat_session_id: seat,
                project_ids: projects.to_vec(),
                allowed_launches: parent_view.grant.allowed_launches.clone(),
                policy,
                child_policy: None,
                max_direct_reports: 4,
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: format!("nest-{label}-{seat}"),
            },
            PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .unwrap()
        .node_id
}

struct Chain {
    store: Store,
    project: Uuid,
    pm: Uuid,
    root: AreaNode,
    parent: AreaNode,
    leaf: AreaNode,
    global: Uuid,
    global_seat: Uuid,
    pinnacle: Uuid,
    pinnacle_seat: Uuid,
    epics: Vec<Uuid>,
}

fn chain() -> Chain {
    let (store, project, root, group, epics) = area_fixture(true);
    let parent = store
        .appoint_area_node(&area_request(
            &store,
            project,
            &root,
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![group],
                epic_ids: vec![],
            },
        ))
        .unwrap();
    let leaf = store
        .appoint_area_node(&area_request(
            &store,
            project,
            &parent,
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![epics[0]],
            },
        ))
        .unwrap();
    let pinnacle_seat = seat(&store, None);
    let pinnacle = root_node(&store, "pinnacle", pinnacle_seat, &[project]);
    let global_seat = seat(&store, None);
    let global = nest(&store, pinnacle, "global", global_seat, &[project]);
    let pm = root.seat_root_session_id;
    assert_eq!(store.global_live_manager(project).unwrap(), Some(pm));
    Chain {
        store,
        project,
        pm,
        root,
        parent,
        leaf,
        global,
        global_seat,
        pinnacle,
        pinnacle_seat,
        epics,
    }
}

fn report(message: &str, key: &str) -> AgentReportUpRequestV1 {
    AgentReportUpRequestV1 {
        message: message.into(),
        idempotency_key: key.into(),
    }
}

fn send(target: ManagerNodeRefV1, key: &str) -> AgentSendDownRequestV1 {
    AgentSendDownRequestV1 {
        target,
        message: "Land #1238 and report back.".into(),
        idempotency_key: key.into(),
    }
}

fn count(store: &Store, sql: &str) -> i64 {
    store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn job_enabled(store: &Store, id: Uuid) -> bool {
    store
        .get_scheduled_job(&id)
        .unwrap()
        .is_some_and(|job| job.enabled)
}

fn message_state(store: &Store, id: Uuid) -> String {
    store
        .conn
        .query_row(
            "SELECT state FROM manager_tier_messages WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

fn grant_of(store: &Store, node: Uuid) -> (i64, i64) {
    let view = store.get_portfolio_node(node).unwrap().unwrap();
    (view.grant.grant_version, view.authority_epoch)
}

/// The leaf escalates to its parent area, and the parent forwards to the
/// project root; returns the escalation at the root.
fn escalate_to_root(c: &Chain, key: &str) -> ManagerNodeEscalationV1 {
    let created = c
        .store
        .create_manager_node_escalation(
            c.leaf.seat_root_session_id,
            &AgentManagerEscalateRequestV1 {
                project_id: c.project,
                subject_id: Uuid::new_v4(),
                reason: "Two areas claim the release branch".into(),
                route: ManagerNodeEscalationRouteV1::Parent,
                expected_source_authority_epoch: c.leaf.authority_epoch,
                expected_source_grant_version: c.leaf.grant_version,
                expected_target_authority_epoch: c.parent.authority_epoch,
                expected_target_grant_version: c.parent.grant_version,
                expected_target_session_id: c.parent.seat_root_session_id,
                idempotency_key: format!("{key}-create"),
            },
        )
        .unwrap();
    let at_root = c
        .store
        .resolve_manager_node_escalation(
            c.parent.seat_root_session_id,
            &AgentManagerResolveEscalationRequestV1 {
                escalation_id: created.id,
                expected_version: created.version,
                expected_target_authority_epoch: c.parent.authority_epoch,
                expected_target_grant_version: c.parent.grant_version,
                expected_target_session_id: c.parent.seat_root_session_id,
                ruling: None,
                idempotency_key: format!("{key}-parent-forward"),
            },
        )
        .unwrap();
    assert_eq!(at_root.target_node_id, c.root.id);
    at_root
}

fn root_forward(
    c: &Chain,
    escalation: &ManagerNodeEscalationV1,
    key: &str,
) -> AgentManagerResolveEscalationRequestV1 {
    AgentManagerResolveEscalationRequestV1 {
        escalation_id: escalation.id,
        expected_version: escalation.version,
        expected_target_authority_epoch: c.root.authority_epoch,
        expected_target_grant_version: c.root.grant_version,
        expected_target_session_id: c.pm,
        ruling: None,
        idempotency_key: key.into(),
    }
}

/// The request a portfolio seat sends for the hop it lists.
fn seat_resolve(
    view: &ManagerNodeEscalationV1,
    ruling: Option<&str>,
    key: &str,
) -> AgentManagerResolveEscalationRequestV1 {
    AgentManagerResolveEscalationRequestV1 {
        escalation_id: view.id,
        expected_version: view.version,
        expected_target_authority_epoch: view.target_authority_epoch,
        expected_target_grant_version: view.target_grant_version,
        expected_target_session_id: view.target_session_id,
        ruling: ruling.map(Into::into),
        idempotency_key: key.into(),
    }
}

fn hop_events(store: &Store, hop: Uuid) -> Vec<String> {
    let mut statement = store
        .conn
        .prepare("SELECT action FROM manager_tier_escalation_events WHERE hop_id=?1 ORDER BY seq")
        .unwrap();
    statement
        .query_map([hop.to_string()], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Acceptance: an area escalation forwarded at every hop reaches the operator
/// queue; the operator's ruling returns to the area seat; every hop is an
/// immutable event; I5: the ruling leaves a WaitingApproval session pending.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_area_escalation_climbs_to_the_operator_and_the_ruling_returns_to_the_area_seat() {
    let c = chain();
    // I5 witness: a worker waiting on a human approval in the project.
    let worker = seat(&c.store, Some(c.project));
    c.store
        .update_session_status(worker, SessionStatus::WaitingApproval)
        .unwrap();
    let approval = Approval {
        id: Uuid::new_v4(),
        session_id: worker,
        tool_name: "Bash".into(),
        tool_input: serde_json::json!({"command": "git push"}),
        status: ApprovalStatus::Pending,
        created_at: Utc::now(),
        resolved_at: None,
    };
    c.store.insert_approval(&approval).unwrap();

    let at_root = escalate_to_root(&c, "climb");
    // The project root crosses into parent_of(Project p): the deepest
    // covering node, `global`.
    let forward = root_forward(&c, &at_root, "climb-root-forward");
    let crossed = c
        .store
        .resolve_manager_node_escalation(c.pm, &forward)
        .unwrap();
    let hop1 = crossed.above_project.clone().expect("the first hop");
    assert_eq!(hop1.hop, 1);
    assert_eq!(hop1.state, "open");
    assert_eq!(hop1.source_ref, format!("project:{}", c.project));
    assert_eq!(hop1.target_ref, format!("portfolio:{}", c.global));
    assert_eq!(hop1.target_session_id, Some(c.global_seat));
    assert_eq!(crossed.state, ManagerNodeEscalationStateV1::Open);
    // A replay of the root's forward returns the same result.
    let replay = c
        .store
        .resolve_manager_node_escalation(c.pm, &forward)
        .unwrap();
    assert_eq!(replay.above_project.unwrap().hop_id, hop1.hop_id);
    // While it is held above, the root can neither rule nor forward it.
    let mut local_ruling = root_forward(&c, &at_root, "climb-root-rule");
    local_ruling.ruling = Some("Root decides".into());
    assert_eq!(
        code(
            c.store
                .resolve_manager_node_escalation(c.pm, &local_ruling)
                .unwrap_err()
        ),
        MANAGER_ESCALATION_FORWARDED_ABOVE
    );
    // The global seat sees it, woken by an escalation message.
    let woken: i64 = c
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_tier_messages WHERE kind='escalation' AND target_session_id=?1",
            [c.global_seat.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(woken, 1);
    let at_global = c
        .store
        .tier_escalations_for_seat(c.global_seat)
        .unwrap()
        .unwrap();
    assert_eq!(at_global.len(), 1);
    let (global_version, global_epoch) = grant_of(&c.store, c.global);
    assert_eq!(at_global[0].target_node_id, c.global);
    assert_eq!(at_global[0].target_grant_version, global_version);
    assert_eq!(at_global[0].target_authority_epoch, global_epoch);
    // The pinnacle is not addressed yet.
    assert!(
        c.store
            .tier_escalations_for_seat(c.pinnacle_seat)
            .unwrap()
            .unwrap()
            .is_empty()
    );
    let mut stale = seat_resolve(&at_global[0], None, "climb-global-stale");
    stale.expected_version += 1;
    assert_eq!(
        code(
            c.store
                .resolve_manager_node_escalation(c.global_seat, &stale)
                .unwrap_err()
        ),
        "manager_node_escalation_stale_target"
    );
    assert_eq!(
        code(
            c.store
                .resolve_manager_node_escalation(
                    c.pinnacle_seat,
                    &seat_resolve(&at_global[0], None, "climb-pinnacle-early")
                )
                .unwrap_err()
        ),
        "manager_node_escalation_not_addressed"
    );
    // global -> pinnacle -> operator.
    let at_pinnacle = c
        .store
        .resolve_manager_node_escalation(
            c.global_seat,
            &seat_resolve(&at_global[0], None, "climb-global-forward"),
        )
        .unwrap();
    let hop2 = at_pinnacle.above_project.unwrap();
    assert_eq!(hop2.hop, 2);
    assert_eq!(hop2.target_ref, format!("portfolio:{}", c.pinnacle));
    let pinnacle_view = c
        .store
        .tier_escalations_for_seat(c.pinnacle_seat)
        .unwrap()
        .unwrap();
    assert_eq!(pinnacle_view.len(), 1);
    let at_operator = c
        .store
        .resolve_manager_node_escalation(
            c.pinnacle_seat,
            &seat_resolve(&pinnacle_view[0], None, "climb-pinnacle-forward"),
        )
        .unwrap();
    let hop3 = at_operator.above_project.unwrap();
    assert_eq!(hop3.hop, 3);
    assert_eq!(hop3.target_ref, OPERATOR_REF);
    assert_eq!(hop3.target_session_id, None);

    // The operator queue holds it.
    let queue = c.store.list_operator_escalations(false).unwrap();
    assert_eq!(queue.escalations.len(), 1);
    assert_eq!(queue.escalations[0].hop_id, hop3.hop_id);
    assert_eq!(queue.escalations[0].escalation_id, at_root.id);
    let rule = RuleOperatorEscalationRequestV1 {
        hop_id: hop3.hop_id,
        ruling: "The leaf area owns the release branch.".into(),
        idempotency_key: "climb-operator-rule".into(),
    };
    let ruled = c.store.rule_operator_escalation(&rule).unwrap();
    assert_eq!(ruled.state, "ruled");
    assert_eq!(ruled.ruling.as_deref(), Some(rule.ruling.as_str()));
    assert_eq!(
        c.store.rule_operator_escalation(&rule).unwrap().hop_id,
        hop3.hop_id,
        "a replay returns the same ruled hop"
    );
    let changed = RuleOperatorEscalationRequestV1 {
        ruling: "Something else".into(),
        ..rule.clone()
    };
    assert_eq!(
        code(c.store.rule_operator_escalation(&changed).unwrap_err()),
        MANAGER_TIER_IDEMPOTENCY_CONFLICT
    );
    assert!(
        c.store
            .list_operator_escalations(false)
            .unwrap()
            .escalations
            .is_empty()
    );

    // The ruling returned down the recorded chain to the leaf's seat.
    let at_leaf = c
        .store
        .list_manager_node_escalations(c.leaf.seat_root_session_id, c.project)
        .unwrap();
    assert_eq!(at_leaf.len(), 1);
    assert_eq!(at_leaf[0].state, ManagerNodeEscalationStateV1::Ruled);
    assert_eq!(at_leaf[0].ruling.as_deref(), Some(rule.ruling.as_str()));
    let ruling_mail: (String, String) = c
        .store
        .conn
        .query_row(
            "SELECT id,target_ref FROM manager_tier_messages WHERE kind='ruling' AND target_session_id=?1",
            [c.leaf.seat_root_session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(ruling_mail.1, format!("area:{}", c.leaf.id));
    let job = c
        .store
        .get_scheduled_job(&Uuid::parse_str(&ruling_mail.0).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(job.wake_session_id, Some(c.leaf.seat_root_session_id));
    assert!(job.message.contains(&rule.ruling));

    // Every hop is an immutable event trail.
    assert_eq!(
        hop_events(&c.store, hop1.hop_id),
        ["opened", "forwarded", "returned"]
    );
    assert_eq!(
        hop_events(&c.store, hop2.hop_id),
        ["opened", "forwarded", "returned"]
    );
    assert_eq!(hop_events(&c.store, hop3.hop_id), ["opened", "ruled"]);
    let in_project: i64 = c
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_node_escalation_events WHERE escalation_id=?1",
            [at_root.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(in_project, 3, "created, forwarded, ruled");
    assert!(
        c.store
            .conn
            .execute(
                "UPDATE manager_tier_escalation_events SET action='ruled' WHERE hop_id=?1",
                [hop1.hop_id.to_string()],
            )
            .is_err()
    );

    // I5: the ruling answered no human approval.
    assert_eq!(
        c.store.get_session(worker).unwrap().unwrap().status,
        SessionStatus::WaitingApproval
    );
    let pending = c.store.get_pending_approvals(worker).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, approval.id);
}

/// I5 at the portfolio tier: a seat's ruling also leaves the approval
/// pending, and the ruling returns to the source.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_portfolio_ruling_returns_to_the_source_and_answers_no_approval() {
    let c = chain();
    let worker = seat(&c.store, Some(c.project));
    c.store
        .update_session_status(worker, SessionStatus::WaitingApproval)
        .unwrap();
    c.store
        .insert_approval(&Approval {
            id: Uuid::new_v4(),
            session_id: worker,
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            status: ApprovalStatus::Pending,
            created_at: Utc::now(),
            resolved_at: None,
        })
        .unwrap();
    let at_root = escalate_to_root(&c, "seat");
    c.store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &at_root, "seat-root-forward"))
        .unwrap();
    let view = c
        .store
        .tier_escalations_for_seat(c.global_seat)
        .unwrap()
        .unwrap();
    let ruled = c
        .store
        .resolve_manager_node_escalation(
            c.global_seat,
            &seat_resolve(&view[0], Some("Split the branch."), "seat-global-rule"),
        )
        .unwrap();
    assert_eq!(ruled.state, ManagerNodeEscalationStateV1::Ruled);
    assert_eq!(ruled.above_project.unwrap().state, "ruled");
    assert_eq!(
        c.store.get_session(worker).unwrap().unwrap().status,
        SessionStatus::WaitingApproval
    );
    assert_eq!(c.store.get_pending_approvals(worker).unwrap().len(), 1);
    assert!(
        c.store
            .list_operator_escalations(true)
            .unwrap()
            .escalations
            .is_empty(),
        "a ruling below the top never reaches the operator queue"
    );
}

/// Acceptance: AgentReportUp climbs exactly one parent at every tier; from a
/// root it is an operator notice and writes no authority row.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn report_up_climbs_one_parent_and_a_root_report_is_an_operator_notice() {
    let c = chain();
    let leaf = c
        .store
        .tier_report_up(c.leaf.seat_root_session_id, &report("leaf done", "r-leaf"))
        .unwrap();
    assert_eq!(leaf.target_ref, format!("area:{}", c.parent.id));
    assert_eq!(leaf.target_session_id, Some(c.parent.seat_root_session_id));
    let parent = c
        .store
        .tier_report_up(
            c.parent.seat_root_session_id,
            &report("area done", "r-parent"),
        )
        .unwrap();
    assert_eq!(parent.target_ref, format!("project:{}", c.project));
    assert_eq!(parent.target_session_id, Some(c.pm));
    let pm = c
        .store
        .tier_report_up(c.pm, &report("project done", "r-pm"))
        .unwrap();
    assert_eq!(pm.target_ref, format!("portfolio:{}", c.global));
    assert_eq!(pm.target_session_id, Some(c.global_seat));
    let job = c.store.get_scheduled_job(&pm.message_id).unwrap().unwrap();
    assert_eq!(job.wake_session_id, Some(c.global_seat));
    assert!(job.message.contains("project done"));
    assert!(c.store.global_message_deliverable(pm.message_id).unwrap());
    let global = c
        .store
        .tier_report_up(c.global_seat, &report("portfolio done", "r-global"))
        .unwrap();
    assert_eq!(global.target_ref, format!("portfolio:{}", c.pinnacle));
    assert_eq!(global.target_session_id, Some(c.pinnacle_seat));

    let authority = |store: &Store| {
        count(
            store,
            "SELECT (SELECT count(*) FROM global_manager_grants)+(SELECT count(*) FROM manager_portfolio_nodes)
                  +(SELECT count(*) FROM manager_portfolio_coverage)+(SELECT count(*) FROM manager_nodes)
                  +(SELECT count(*) FROM manager_node_grants)+(SELECT count(*) FROM harness_manager_scopes)
                  +(SELECT count(*) FROM harness_manager_v2_policies)",
        )
    };
    let before = authority(&c.store);
    let top = c
        .store
        .tier_report_up(c.pinnacle_seat, &report("pinnacle done", "r-top"))
        .unwrap();
    assert_eq!(top.target_ref, OPERATOR_REF);
    assert_eq!(top.target_session_id, None);
    assert!(
        c.store
            .get_scheduled_job(&top.message_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(authority(&c.store), before, "a report carries no authority");
    let notices = c.store.list_operator_escalations(false).unwrap().notices;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].message_id, top.message_id);
    assert_eq!(notices[0].body, "pinnacle done");
    assert_eq!(notices[0].source_ref, format!("portfolio:{}", c.pinnacle));
    let read = c.store.acknowledge_operator_notice(top.message_id).unwrap();
    assert_eq!(read.state, "delivered");
    assert!(
        c.store
            .list_operator_escalations(false)
            .unwrap()
            .notices
            .is_empty()
    );
    assert_eq!(
        c.store
            .list_operator_escalations(true)
            .unwrap()
            .notices
            .len(),
        1
    );
    assert_eq!(
        code(
            c.store
                .acknowledge_operator_notice(pm.message_id)
                .unwrap_err()
        ),
        OPERATOR_NOTICE_NOT_FOUND
    );

    // A replay under the same key returns the same row; a changed body under
    // it is refused.
    let replay = c
        .store
        .tier_report_up(c.pm, &report("project done", "r-pm"))
        .unwrap();
    assert_eq!(replay.message_id, pm.message_id);
    assert!(replay.deduplicated);
    assert_eq!(
        code(
            c.store
                .tier_report_up(c.pm, &report("changed", "r-pm"))
                .unwrap_err()
        ),
        MANAGER_TIER_IDEMPOTENCY_CONFLICT
    );
    // A session holding no node seat cannot report up.
    let worker = seat(&c.store, Some(c.project));
    assert_eq!(
        code(
            c.store
                .tier_report_up(worker, &report("hi", "r-worker"))
                .unwrap_err()
        ),
        MANAGER_TIER_NOT_NODE_SEAT
    );
}

/// Acceptance: AgentSendDown reaches a grandchild (and deeper) seat in
/// coverage and is refused upward, to itself, to siblings and outside
/// coverage.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn send_down_reaches_descendants_in_coverage_only() {
    let c = chain();
    let other = project(&c.store, "Other");
    let sibling_seat = seat(&c.store, None);
    let sibling = root_node(&c.store, "sibling", sibling_seat, &[other]);

    // pinnacle -> project (a grandchild), -> leaf area (four levels down),
    // -> its child node.
    let grandchild = c
        .store
        .tier_send_down(
            c.pinnacle_seat,
            &send(
                ManagerNodeRefV1::Project {
                    project_id: c.project,
                },
                "d-project",
            ),
        )
        .unwrap();
    assert_eq!(grandchild.target_session_id, Some(c.pm));
    let job = c
        .store
        .get_scheduled_job(&grandchild.message_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.wake_session_id, Some(c.pm));
    assert!(
        c.store
            .global_message_deliverable(grandchild.message_id)
            .unwrap()
    );
    let deep = c
        .store
        .tier_send_down(
            c.pinnacle_seat,
            &send(ManagerNodeRefV1::Area { node_id: c.leaf.id }, "d-leaf"),
        )
        .unwrap();
    assert_eq!(deep.target_session_id, Some(c.leaf.seat_root_session_id));
    let child = c
        .store
        .tier_send_down(
            c.pinnacle_seat,
            &send(
                ManagerNodeRefV1::Portfolio { node_id: c.global },
                "d-global",
            ),
        )
        .unwrap();
    assert_eq!(child.target_session_id, Some(c.global_seat));
    let area = c
        .store
        .tier_send_down(
            c.parent.seat_root_session_id,
            &send(ManagerNodeRefV1::Area { node_id: c.leaf.id }, "d-area"),
        )
        .unwrap();
    assert_eq!(area.target_ref, format!("area:{}", c.leaf.id));

    for (caller, target) in [
        (
            c.global_seat,
            ManagerNodeRefV1::Portfolio {
                node_id: c.pinnacle,
            },
        ),
        (c.pm, ManagerNodeRefV1::Portfolio { node_id: c.global }),
        (
            c.pm,
            ManagerNodeRefV1::Project {
                project_id: c.project,
            },
        ),
        (
            c.leaf.seat_root_session_id,
            ManagerNodeRefV1::Area {
                node_id: c.parent.id,
            },
        ),
        (
            c.pinnacle_seat,
            ManagerNodeRefV1::Portfolio {
                node_id: c.pinnacle,
            },
        ),
    ] {
        assert_eq!(
            code(
                c.store
                    .tier_send_down(caller, &send(target, "d-up"))
                    .unwrap_err()
            ),
            MANAGER_TARGET_NOT_DESCENDANT,
            "{target:?}"
        );
    }
    for (caller, target) in [
        (
            c.pinnacle_seat,
            ManagerNodeRefV1::Project { project_id: other },
        ),
        (
            sibling_seat,
            ManagerNodeRefV1::Project {
                project_id: c.project,
            },
        ),
        (
            sibling_seat,
            ManagerNodeRefV1::Portfolio { node_id: c.global },
        ),
        (
            c.global_seat,
            ManagerNodeRefV1::Portfolio { node_id: sibling },
        ),
        (sibling_seat, ManagerNodeRefV1::Area { node_id: c.leaf.id }),
    ] {
        assert_eq!(
            code(
                c.store
                    .tier_send_down(caller, &send(target, "d-out"))
                    .unwrap_err()
            ),
            rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE,
            "{target:?}"
        );
    }
    let worker = seat(&c.store, Some(c.project));
    assert_eq!(
        code(
            c.store
                .tier_send_down(
                    worker,
                    &send(ManagerNodeRefV1::Area { node_id: c.leaf.id }, "d-worker")
                )
                .unwrap_err()
        ),
        MANAGER_TIER_NOT_NODE_SEAT
    );
    assert_eq!(
        code(
            c.store
                .tier_send_down(
                    c.pinnacle_seat,
                    &send(
                        ManagerNodeRefV1::Area {
                            node_id: Uuid::new_v4()
                        },
                        "d-unknown"
                    )
                )
                .unwrap_err()
        ),
        MANAGER_TIER_TARGET_UNKNOWN
    );
}

/// Acceptance: revoking or replacing a node retires its queued tier mail and
/// its open hops; the delivery fence fails for them; a retired hop returns
/// the escalation to the root, which may forward it again.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn revoking_or_replacing_a_node_retires_its_queued_mail_and_open_hops() {
    let c = chain();
    let up = c
        .store
        .tier_report_up(c.pm, &report("for global", "x-up"))
        .unwrap();
    let down = c
        .store
        .tier_send_down(
            c.pinnacle_seat,
            &send(ManagerNodeRefV1::Portfolio { node_id: c.global }, "x-down"),
        )
        .unwrap();
    let from_global = c
        .store
        .tier_send_down(
            c.global_seat,
            &send(
                ManagerNodeRefV1::Project {
                    project_id: c.project,
                },
                "x-from",
            ),
        )
        .unwrap();
    let to_pinnacle = c
        .store
        .tier_report_up(c.global_seat, &report("for pinnacle", "x-pin"))
        .unwrap();
    let at_root = escalate_to_root(&c, "x");
    let crossed = c
        .store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &at_root, "x-root-forward"))
        .unwrap();
    let hop = crossed.above_project.unwrap();
    for id in [up.message_id, down.message_id, from_global.message_id] {
        assert!(c.store.global_message_deliverable(id).unwrap());
    }

    let (version, epoch) = grant_of(&c.store, c.global);
    c.store
        .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
            node_id: c.global,
            expected_grant_version: version,
            expected_authority_epoch: epoch,
            idempotency_key: "x-revoke-global".into(),
        })
        .unwrap();
    for id in [
        up.message_id,
        down.message_id,
        from_global.message_id,
        to_pinnacle.message_id,
    ] {
        assert_eq!(message_state(&c.store, id), "retired");
        assert!(!job_enabled(&c.store, id));
        assert!(!c.store.global_message_deliverable(id).unwrap());
    }
    assert_eq!(hop_events(&c.store, hop.hop_id), ["opened", "retired"]);
    let back = c
        .store
        .list_manager_node_escalations(c.pm, c.project)
        .unwrap()
        .into_iter()
        .find(|e| e.id == at_root.id)
        .unwrap();
    assert_eq!(back.above_project.as_ref().unwrap().state, "retired");
    // The root forwards again: parent_of(Project p) is now the pinnacle.
    let again = c
        .store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &back, "x-root-again"))
        .unwrap()
        .above_project
        .unwrap();
    assert_eq!(again.hop, 2);
    assert_eq!(again.target_ref, format!("portfolio:{}", c.pinnacle));

    // Replacing the pinnacle's seat retires mail and hops addressed to it.
    let to_top = c
        .store
        .tier_report_up(c.pm, &report("for pinnacle now", "x-up-2"))
        .unwrap();
    assert_eq!(to_top.target_session_id, Some(c.pinnacle_seat));
    let (version, epoch) = grant_of(&c.store, c.pinnacle);
    let new_seat = seat(&c.store, None);
    let view = c.store.get_portfolio_node(c.pinnacle).unwrap().unwrap();
    c.store
        .configure_portfolio_node(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: Some(c.pinnacle),
                parent_node_id: None,
                adopt_node_ids: vec![],
                expected_parent_grant_version: None,
                tier_label: "pinnacle".into(),
                seat_session_id: new_seat,
                project_ids: view.grant.project_ids.clone(),
                allowed_launches: view.grant.allowed_launches.clone(),
                policy: view.grant.project_policy.clone(),
                child_policy: None,
                max_direct_reports: 5,
                expected_node_grant_version: version,
                expected_authority_epoch: epoch,
                idempotency_key: "x-replace-pinnacle".into(),
            },
            PortfolioGrantor::Operator,
            "operator:test",
        )
        .unwrap();
    assert_eq!(message_state(&c.store, to_top.message_id), "retired");
    assert!(!job_enabled(&c.store, to_top.message_id));
    assert_eq!(hop_events(&c.store, again.hop_id), ["opened", "retired"]);
}

/// PM displacement retires the queued down-mail to the seat and its own
/// reports; an area node's revocation fails its mail's delivery fence.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn seat_displacement_at_either_end_fails_the_delivery_fence() {
    let c = chain();
    let to_pm = c
        .store
        .tier_send_down(
            c.global_seat,
            &send(
                ManagerNodeRefV1::Project {
                    project_id: c.project,
                },
                "s-pm",
            ),
        )
        .unwrap();
    let to_leaf = c
        .store
        .tier_send_down(
            c.pm,
            &send(ManagerNodeRefV1::Area { node_id: c.leaf.id }, "s-leaf"),
        )
        .unwrap();
    let pm_report = c
        .store
        .tier_report_up(c.pm, &report("status", "s-report"))
        .unwrap();
    assert!(
        c.store
            .global_message_deliverable(to_leaf.message_id)
            .unwrap()
    );
    c.store
        .revoke_area_node(&RevokeAreaNode {
            idempotency_key: None,
            project_id: c.project,
            node_id: c.leaf.id,
            expected_grant_version: c.leaf.grant_version,
            expected_epoch: c.leaf.authority_epoch,
            operator_origin: "operator-test".into(),
        })
        .unwrap();
    assert!(
        !c.store
            .global_message_deliverable(to_leaf.message_id)
            .unwrap()
    );

    c.store
        .retire_tier_project_seat(c.project, &stamp())
        .unwrap();
    for id in [to_pm.message_id, pm_report.message_id] {
        assert_eq!(message_state(&c.store, id), "retired");
        assert!(!job_enabled(&c.store, id));
    }
}

/// Lead -> manager mail in a project with no PM reaches the deepest covering
/// portfolio node; with a PM the PM path keeps it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn lead_mail_without_a_pm_reaches_the_covering_portfolio_node() {
    let c = chain();
    let unmanaged = project(&c.store, "Unmanaged");
    let node_seat = seat(&c.store, None);
    let node = root_node(&c.store, "global-two", node_seat, &[unmanaged]);
    let mut group = test_session(Uuid::new_v4(), PathBuf::from("/tmp/tier-routing"));
    group.project_id = Some(unmanaged);
    group.session_kind = SessionKind::Group;
    c.store.insert_session(&group).unwrap();
    let lead = Uuid::new_v4();
    let mut epic = test_session(Uuid::new_v4(), PathBuf::from("/tmp/tier-routing"));
    epic.project_id = Some(unmanaged);
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    epic.lead_session_id = Some(lead);
    c.store.insert_session(&epic).unwrap();
    let mut lead_row = test_session(lead, PathBuf::from("/tmp/tier-routing"));
    lead_row.project_id = Some(unmanaged);
    lead_row.parent_id = Some(epic.id);
    c.store.insert_session(&lead_row).unwrap();

    let receipt = c
        .store
        .tier_lead_notice(lead, "blocked on review", "lead-1")
        .unwrap()
        .expect("routed to the covering node");
    assert_eq!(receipt.target_ref, format!("portfolio:{node}"));
    assert_eq!(receipt.target_session_id, Some(node_seat));
    let job = c
        .store
        .get_scheduled_job(&receipt.message_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.wake_session_id, Some(node_seat));
    assert!(job.message.contains("blocked on review"));
    // The catalog advertises the route to that lead.
    let projection = c.store.agent_authority_projection(lead).unwrap();
    assert!(
        projection
            .verbs
            .contains(&rsi_common::agent_control_schema::AgentControlVerbV1::ManagerNotify)
    );
    // A non-lead in the same project is not routed.
    let stranger = seat(&c.store, Some(unmanaged));
    assert!(
        c.store
            .tier_lead_notice(stranger, "hi", "lead-2")
            .unwrap()
            .is_none()
    );
    // A project with a live PM keeps the PM mail path.
    let worker = seat(&c.store, Some(c.project));
    assert!(
        c.store
            .tier_lead_notice(worker, "hi", "lead-3")
            .unwrap()
            .is_none()
    );
}

/// The aliases keep v0's receipts and codes; under N levels the PM's report
/// reaches the deepest covering node, and a seat may mail a covered project
/// several levels down.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_v0_aliases_route_through_tier_mail_with_v0_receipts() {
    let c = chain();
    let report = c
        .store
        .tier_report_to_global(
            c.pm,
            &AgentReportToGlobalRequestV1 {
                message: "Landed.".into(),
                idempotency_key: "a-report".into(),
            },
        )
        .unwrap();
    assert_eq!(report.project_id, c.project);
    assert_eq!(report.target_session_id, c.global_seat);
    assert!(!report.deduplicated);
    let send = c
        .store
        .tier_global_send(
            c.pinnacle_seat,
            &AgentGlobalSendRequestV1 {
                project_id: c.project,
                message: "Status?".into(),
                idempotency_key: "a-send".into(),
            },
        )
        .unwrap();
    assert_eq!(send.target_session_id, c.pm);
    let replay = c
        .store
        .tier_global_send(
            c.pinnacle_seat,
            &AgentGlobalSendRequestV1 {
                project_id: c.project,
                message: "Status?".into(),
                idempotency_key: "a-send".into(),
            },
        )
        .unwrap();
    assert_eq!(replay.message_id, send.message_id);
    assert!(replay.deduplicated);
    assert_eq!(
        code(
            c.store
                .tier_global_send(
                    c.pinnacle_seat,
                    &AgentGlobalSendRequestV1 {
                        project_id: c.project,
                        message: "Changed".into(),
                        idempotency_key: "a-send".into(),
                    },
                )
                .unwrap_err()
        ),
        GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT
    );
    assert_eq!(
        code(
            c.store
                .tier_global_send(
                    c.pm,
                    &AgentGlobalSendRequestV1 {
                        project_id: c.project,
                        message: "Status?".into(),
                        idempotency_key: "a-pm".into(),
                    },
                )
                .unwrap_err()
        ),
        GLOBAL_MANAGER_NOT_SEAT
    );
    let rows: i64 = count(&c.store, "SELECT count(*) FROM global_manager_messages");
    assert_eq!(rows, 0, "new writes leave global_manager_messages");
}

/// Migration retention: tier rows are never deleted, identity columns never
/// change, and closed rows are final.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn tier_rows_refuse_delete_identity_edits_and_reopening() {
    let c = chain();
    let mail = c
        .store
        .tier_report_up(c.pm, &report("keep", "k-mail"))
        .unwrap();
    let id = mail.message_id.to_string();
    let conn = &c.store.conn;
    assert!(
        conn.execute("DELETE FROM manager_tier_messages WHERE id=?1", [&id])
            .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE manager_tier_messages SET body='forged' WHERE id=?1",
            [&id]
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE manager_tier_messages SET target_ref='operator',target_session_id=NULL WHERE id=?1",
            [&id]
        )
        .is_err()
    );
    conn.execute(
        "UPDATE manager_tier_messages SET state='delivered' WHERE id=?1",
        [&id],
    )
    .unwrap();
    assert!(
        conn.execute(
            "UPDATE manager_tier_messages SET state='queued' WHERE id=?1",
            [&id]
        )
        .is_err()
    );
    // #1266: claimed -> queued never reopens; failed and uncertain carry a
    // reason and are final, reason included.
    let other = c
        .store
        .tier_report_up(c.pm, &report("keep two", "k-mail-2"))
        .unwrap()
        .message_id
        .to_string();
    conn.execute(
        "UPDATE manager_tier_messages SET state='claimed' WHERE id=?1",
        [&other],
    )
    .unwrap();
    for refused in [
        "UPDATE manager_tier_messages SET state='queued' WHERE id=?1",
        "UPDATE manager_tier_messages SET state='retired' WHERE id=?1",
        "UPDATE manager_tier_messages SET state='failed' WHERE id=?1",
    ] {
        assert!(conn.execute(refused, [&other]).is_err(), "{refused}");
    }
    conn.execute(
        "UPDATE manager_tier_messages SET state='failed',settle_reason='admission refused' WHERE id=?1",
        [&other],
    )
    .unwrap();
    for refused in [
        "UPDATE manager_tier_messages SET state='uncertain' WHERE id=?1",
        "UPDATE manager_tier_messages SET state='delivered',settle_reason=NULL WHERE id=?1",
        "UPDATE manager_tier_messages SET settle_reason='rewritten' WHERE id=?1",
        "DELETE FROM manager_tier_messages WHERE id=?1",
    ] {
        assert!(conn.execute(refused, [&other]).is_err(), "{refused}");
    }
    let third = c
        .store
        .tier_report_up(c.pm, &report("keep three", "k-mail-3"))
        .unwrap()
        .message_id
        .to_string();
    conn.execute(
        "UPDATE manager_tier_messages SET state='claimed' WHERE id=?1",
        [&third],
    )
    .unwrap();
    conn.execute(
        "UPDATE manager_tier_messages SET state='uncertain',settle_reason='daemon_restarted' WHERE id=?1",
        [&third],
    )
    .unwrap();
    assert!(
        conn.execute(
            "UPDATE manager_tier_messages SET state='failed',settle_reason='x' WHERE id=?1",
            [&third]
        )
        .is_err()
    );

    let at_root = escalate_to_root(&c, "k");
    let hop = c
        .store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &at_root, "k-forward"))
        .unwrap()
        .above_project
        .unwrap();
    let hop_id = hop.hop_id.to_string();
    assert!(
        conn.execute(
            "DELETE FROM manager_tier_escalations WHERE id=?1",
            [&hop_id]
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE manager_tier_escalations SET target_ref='operator',target_session_id=NULL,target_grant_version=NULL WHERE id=?1",
            [&hop_id]
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "DELETE FROM manager_tier_escalation_events WHERE hop_id=?1",
            [&hop_id]
        )
        .is_err()
    );
    conn.execute(
        "UPDATE manager_tier_escalations SET state='retired' WHERE id=?1",
        [&hop_id],
    )
    .unwrap();
    assert!(
        conn.execute(
            "UPDATE manager_tier_escalations SET state='open' WHERE id=?1",
            [&hop_id]
        )
        .is_err()
    );
    // A second open hop for one escalation is refused by the index.
    let second = conn.execute(
        "INSERT INTO manager_tier_escalations(id,escalation_id,project_id,hop,source_ref,target_ref,actor_session_id,target_session_id,target_grant_version,state,ruling,created_at,updated_at)
         SELECT ?1,escalation_id,project_id,hop+1,source_ref,'operator',actor_session_id,NULL,NULL,'open',NULL,created_at,updated_at FROM manager_tier_escalations WHERE id=?2",
        params![Uuid::new_v4().to_string(), hop_id],
    );
    second.unwrap();
    let third = conn.execute(
        "INSERT INTO manager_tier_escalations(id,escalation_id,project_id,hop,source_ref,target_ref,actor_session_id,target_session_id,target_grant_version,state,ruling,created_at,updated_at)
         SELECT ?1,escalation_id,project_id,hop+2,source_ref,'operator',actor_session_id,NULL,NULL,'open',NULL,created_at,updated_at FROM manager_tier_escalations WHERE id=?2",
        params![Uuid::new_v4().to_string(), hop_id],
    );
    assert!(third.is_err(), "one open hop per escalation");
    // A malformed node reference is refused.
    assert!(
        conn.execute(
            "INSERT INTO manager_tier_messages(id,direction,kind,source_ref,target_ref,project_id,source_session_id,target_session_id,source_grant_version,target_grant_version,body,body_digest,idempotency_key,state,created_at,updated_at)
             SELECT ?1,direction,kind,'epic:x',target_ref,project_id,source_session_id,target_session_id,source_grant_version,target_grant_version,body,body_digest,'k-forged',state,created_at,updated_at FROM manager_tier_messages WHERE id=?2",
            params![Uuid::new_v4().to_string(), id],
        )
        .is_err()
    );
}

fn store_at_v154() -> Store {
    let store = super::super::raw_in_memory_store_for_test().unwrap();
    for &(step, migrate) in super::super::MIGRATION_STEPS {
        if step >= TIER_ROUTING_SCHEMA_VERSION {
            break;
        }
        migrate(&store, 0).unwrap();
    }
    store
}

fn tier_catalog(store: &Store) -> Vec<(String, String)> {
    let mut statement = store
        .conn
        .prepare("SELECT type,name FROM sqlite_master WHERE tbl_name LIKE 'manager_tier_%' ORDER BY type,name")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// M2 installs exactly its catalog at V155, refuses a re-apply, and the
/// rewind fixture restores V154 and replays.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn v155_installs_the_tier_catalog_and_rewinds_exactly() {
    let store = store_at_v154();
    assert!(tier_catalog(&store).is_empty());
    store.migrate_v155(TIER_ROUTING_SCHEMA_VERSION - 1).unwrap();
    let version: i32 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, TIER_ROUTING_SCHEMA_VERSION);
    let mut expected: Vec<(String, String)> = CATALOG_OBJECTS
        .iter()
        .map(|(kind, name)| ((*kind).to_string(), (*name).to_string()))
        .collect();
    expected.sort();
    let installed: Vec<(String, String)> = tier_catalog(&store)
        .into_iter()
        .filter(|(_, name)| !name.starts_with("sqlite_autoindex"))
        .collect();
    assert_eq!(installed, expected);
    store.migrate_v155(TIER_ROUTING_SCHEMA_VERSION).unwrap();
    assert!(apply_migration(&store, TIER_ROUTING_SCHEMA_VERSION).is_err());

    let head = Store::open_in_memory().unwrap();
    rewind_to_v154(&head.conn).unwrap();
    assert!(tier_catalog(&head).is_empty());
    head.migrate_v155(TIER_ROUTING_SCHEMA_VERSION - 1).unwrap();
    assert_eq!(
        tier_catalog(&head)
            .into_iter()
            .filter(|(_, name)| !name.starts_with("sqlite_autoindex"))
            .count(),
        CATALOG_OBJECTS.len()
    );
}

/// #1266: a claim refused for one stale message leaves the batch's current
/// tier message queued; it is delivered by the next claim that succeeds.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_refused_claim_keeps_the_batchs_current_tier_mail_queued() {
    let c = chain();
    let stale = c
        .store
        .tier_send_down(
            c.global_seat,
            &send(
                ManagerNodeRefV1::Project {
                    project_id: c.project,
                },
                "c-stale",
            ),
        )
        .unwrap();
    let current = c
        .store
        .tier_send_down(
            c.pinnacle_seat,
            &send(
                ManagerNodeRefV1::Project {
                    project_id: c.project,
                },
                "c-current",
            ),
        )
        .unwrap();
    let (version, epoch) = grant_of(&c.store, c.global);
    c.store
        .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
            node_id: c.global,
            expected_grant_version: version,
            expected_authority_epoch: epoch,
            idempotency_key: "c-revoke".into(),
        })
        .unwrap();
    // The continuation effect claim commits even when it refuses (its
    // retirements must persist), exactly as `claim_continuation_effect` does.
    let claim = |jobs: &[Uuid]| {
        let tx = Transaction::new_unchecked(&c.store.conn, TransactionBehavior::Immediate).unwrap();
        let claimed = c.store.claim_global_messages_for_tip(jobs, c.pm).unwrap();
        tx.commit().unwrap();
        claimed
    };
    assert!(!claim(&[stale.message_id, current.message_id]));
    assert_eq!(message_state(&c.store, stale.message_id), "retired");
    assert_eq!(message_state(&c.store, current.message_id), "queued");
    assert!(job_enabled(&c.store, current.message_id));
    assert!(
        c.store
            .global_message_deliverable(current.message_id)
            .unwrap()
    );
    assert!(claim(&[current.message_id]));
    // #1266: a successful claim reserves the message for its continuation;
    // only that continuation's settlement records the outcome.
    assert_eq!(message_state(&c.store, current.message_id), "claimed");
    assert!(
        !c.store
            .global_message_deliverable(current.message_id)
            .unwrap()
    );
    assert_eq!(
        c.store
            .settle_tier_messages(&[current.message_id], TierMailSettlement::Delivered, None)
            .unwrap(),
        1
    );
    assert_eq!(message_state(&c.store, current.message_id), "delivered");
    assert!(!job_enabled(&c.store, current.message_id));
}

fn settle_reason(store: &Store, id: Uuid) -> Option<String> {
    store
        .conn
        .query_row(
            "SELECT settle_reason FROM manager_tier_messages WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

/// The effect claim, committed the way `claim_continuation_effect` commits.
fn claim_for(c: &Chain, jobs: &[Uuid], tip: Uuid) -> bool {
    let tx = Transaction::new_unchecked(&c.store.conn, TransactionBehavior::Immediate).unwrap();
    let claimed = c.store.claim_global_messages_for_tip(jobs, tip).unwrap();
    tx.commit().unwrap();
    claimed
}

/// #1266 (the #945 rule): a continuation that claimed tier mail and then
/// failed admission settles it `failed` with the reason. It is not
/// delivered, its wake is off, nothing replays or reopens it, and its
/// sender, its recipient and the operator all see it with the reason.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_claim_whose_admission_fails_settles_failed_and_visible_without_replay() {
    let c = chain();
    let mail = c
        .store
        .tier_send_down(
            c.pinnacle_seat,
            &send(
                ManagerNodeRefV1::Project {
                    project_id: c.project,
                },
                "f-mail",
            ),
        )
        .unwrap();
    let id = mail.message_id;
    assert!(claim_for(&c, &[id], c.pm));
    assert_eq!(message_state(&c.store, id), "claimed");
    // A restart's reconciliation is the only other way out of `claimed`;
    // the in-flight claim is not retired by the scheduler's precheck.
    c.store.retire_global_message(id).unwrap();
    assert_eq!(message_state(&c.store, id), "claimed");

    let reason = "model admission refused: capacity_exhausted";
    assert_eq!(
        c.store
            .settle_tier_messages(&[id], TierMailSettlement::Failed, Some(reason))
            .unwrap(),
        1
    );
    assert_eq!(message_state(&c.store, id), "failed");
    assert_eq!(settle_reason(&c.store, id).as_deref(), Some(reason));
    assert!(!job_enabled(&c.store, id));
    assert!(!c.store.global_message_deliverable(id).unwrap());

    // No replay: a later claim refuses, and no settlement changes it.
    assert!(!claim_for(&c, &[id], c.pm));
    assert_eq!(
        c.store
            .settle_tier_messages(&[id], TierMailSettlement::Delivered, None)
            .unwrap(),
        0
    );
    assert_eq!(message_state(&c.store, id), "failed");
    assert!(!job_enabled(&c.store, id));

    // Visible with its reason: recipient inbox, sender, operator view.
    let inbox = c
        .store
        .manager_inbox(c.pm, &AgentManagerInboxRequestV1::default())
        .unwrap();
    let received = inbox
        .undelivered_tier_mail
        .iter()
        .find(|row| row.message_id == id)
        .expect("the recipient sees its failed mail");
    assert_eq!(received.role, "recipient");
    assert_eq!(received.state, "failed");
    assert_eq!(received.settle_reason, reason);
    assert_eq!(received.body, "Land #1238 and report back.");
    let sent = c
        .store
        .undelivered_tier_mail_for_session(c.pinnacle_seat)
        .unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].message_id, id);
    assert_eq!(sent[0].role, "sender");
    assert_eq!(sent[0].settle_reason, reason);
    let operator = c.store.list_operator_escalations(false).unwrap();
    let shown = operator
        .undelivered
        .iter()
        .find(|row| row.message_id == id)
        .expect("the operator sees failed mail");
    assert_eq!(shown.state, "failed");
    assert_eq!(shown.settle_reason, reason);
    assert_eq!(shown.target_ref, format!("project:{}", c.project));
}

/// #1266: a crash between the claim and the launch leaves the message
/// `claimed`; startup reconciliation settles it `uncertain` (the provider
/// may have seen it), disables its wake and leaves queued mail queued.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_claim_cut_short_by_a_restart_settles_uncertain() {
    let c = chain();
    let target = ManagerNodeRefV1::Project {
        project_id: c.project,
    };
    let claimed = c
        .store
        .tier_send_down(c.pinnacle_seat, &send(target.clone(), "u-claimed"))
        .unwrap()
        .message_id;
    let waiting = c
        .store
        .tier_send_down(c.pinnacle_seat, &send(target, "u-waiting"))
        .unwrap()
        .message_id;
    assert!(claim_for(&c, &[claimed], c.pm));
    assert_eq!(c.store.reconcile_tier_messages_at_startup().unwrap(), 1);
    assert_eq!(message_state(&c.store, claimed), "uncertain");
    assert_eq!(
        settle_reason(&c.store, claimed).as_deref(),
        Some(TIER_RESTART_REASON)
    );
    assert!(!job_enabled(&c.store, claimed));
    assert!(!c.store.global_message_deliverable(claimed).unwrap());
    assert_eq!(message_state(&c.store, waiting), "queued");
    assert!(job_enabled(&c.store, waiting));
    assert_eq!(settle_reason(&c.store, waiting), None);
    // Idempotent: a second startup finds nothing claimed.
    assert_eq!(c.store.reconcile_tier_messages_at_startup().unwrap(), 0);
    let shown = c.store.list_operator_escalations(false).unwrap();
    assert!(
        shown
            .undelivered
            .iter()
            .any(|row| row.message_id == claimed && row.state == "uncertain")
    );
}

/// #1267: a pre-#1238 message replays under its key only with the same
/// request; a changed request is the v0 idempotency conflict.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_pre_upgrade_alias_key_replays_only_the_same_request() {
    use crate::store::global_manager::{GlobalMessage, GlobalMessageDirection};
    let c = chain();
    let grant = c
        .store
        .portfolio_seat_grant(c.global_seat)
        .unwrap()
        .unwrap();
    let request = |message: &str| AgentReportToGlobalRequestV1 {
        message: message.into(),
        idempotency_key: "legacy-report".into(),
    };
    let legacy = c
        .store
        .queue_global_message(&GlobalMessage {
            grant: &grant,
            direction: GlobalMessageDirection::ToGlobal,
            project_id: c.project,
            sender: c.pm,
            target: c.global_seat,
            idempotency_key: "legacy-report",
            request: serde_json::to_value(request("Landed.")).unwrap(),
            delivery: "Report".into(),
        })
        .unwrap();
    let replay = c
        .store
        .tier_report_to_global(c.pm, &request("Landed."))
        .unwrap();
    assert_eq!(replay.message_id, legacy.message_id);
    assert!(replay.deduplicated);
    assert_eq!(
        code(
            c.store
                .tier_report_to_global(c.pm, &request("Changed."))
                .unwrap_err()
        ),
        GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT
    );
    // The send alias keeps the same rule for its pre-upgrade rows.
    let send_request = |message: &str| AgentGlobalSendRequestV1 {
        project_id: c.project,
        message: message.into(),
        idempotency_key: "legacy-send".into(),
    };
    let pinnacle = c
        .store
        .portfolio_seat_grant(c.pinnacle_seat)
        .unwrap()
        .unwrap();
    c.store
        .queue_global_message(&GlobalMessage {
            grant: &pinnacle,
            direction: GlobalMessageDirection::ToManager,
            project_id: c.project,
            sender: c.pinnacle_seat,
            target: c.pm,
            idempotency_key: "legacy-send",
            request: serde_json::to_value(send_request("Status?")).unwrap(),
            delivery: "Message".into(),
        })
        .unwrap();
    assert!(
        c.store
            .tier_global_send(c.pinnacle_seat, &send_request("Status?"))
            .unwrap()
            .deduplicated
    );
    assert_eq!(
        code(
            c.store
                .tier_global_send(c.pinnacle_seat, &send_request("Other"))
                .unwrap_err()
        ),
        GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT
    );
}

/// #1268: a portfolio seat's identical retry replays after its forward and
/// after its ruling, though the hop it acted on is closed by then.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_portfolio_seats_identical_retry_replays_after_a_forward_and_a_ruling() {
    let c = chain();
    let at_root = escalate_to_root(&c, "r");
    c.store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &at_root, "r-root"))
        .unwrap();
    let at_global = c
        .store
        .tier_escalations_for_seat(c.global_seat)
        .unwrap()
        .unwrap();
    let forward = seat_resolve(&at_global[0], None, "r-global-forward");
    let forwarded = c
        .store
        .resolve_manager_node_escalation(c.global_seat, &forward)
        .unwrap();
    let retry = c
        .store
        .resolve_manager_node_escalation(c.global_seat, &forward)
        .unwrap();
    assert_eq!(
        retry.above_project.unwrap().hop_id,
        forwarded.above_project.clone().unwrap().hop_id
    );
    let at_pinnacle = c
        .store
        .tier_escalations_for_seat(c.pinnacle_seat)
        .unwrap()
        .unwrap();
    let ruling = seat_resolve(&at_pinnacle[0], Some("Split it."), "r-pinnacle-rule");
    let ruled = c
        .store
        .resolve_manager_node_escalation(c.pinnacle_seat, &ruling)
        .unwrap();
    assert_eq!(ruled.state, ManagerNodeEscalationStateV1::Ruled);
    let again = c
        .store
        .resolve_manager_node_escalation(c.pinnacle_seat, &ruling)
        .unwrap();
    assert_eq!(again.state, ManagerNodeEscalationStateV1::Ruled);
    assert_eq!(again.version, ruled.version);
    // The global seat's forward still replays after the ruling.
    assert_eq!(
        c.store
            .resolve_manager_node_escalation(c.global_seat, &forward)
            .unwrap()
            .above_project
            .unwrap()
            .hop,
        2
    );
    // A changed request under a used key is refused.
    let mut changed = forward.clone();
    changed.ruling = Some("Different".into());
    assert_eq!(
        code(
            c.store
                .resolve_manager_node_escalation(c.global_seat, &changed)
                .unwrap_err()
        ),
        "manager_node_idempotency_conflict"
    );
}

/// #1269: the source fences hold above the project root. Revoking the
/// source area retires its open hop, and the global seat can no longer act;
/// a source re-appointed under a new grant retires the hop on the next
/// resolve and refuses it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_revoked_or_regranted_source_fences_the_hops_above_the_root() {
    let c = chain();
    let at_root = escalate_to_root(&c, "v");
    let hop = c
        .store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &at_root, "v-root"))
        .unwrap()
        .above_project
        .unwrap();
    let at_global = c
        .store
        .tier_escalations_for_seat(c.global_seat)
        .unwrap()
        .unwrap();
    c.store
        .revoke_area_node(&RevokeAreaNode {
            idempotency_key: None,
            project_id: c.project,
            node_id: c.leaf.id,
            expected_grant_version: c.leaf.grant_version,
            expected_epoch: c.leaf.authority_epoch,
            operator_origin: "operator-test".into(),
        })
        .unwrap();
    assert_eq!(hop_events(&c.store, hop.hop_id), ["opened", "retired"]);
    assert!(
        c.store
            .tier_escalations_for_seat(c.global_seat)
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert!(
        c.store
            .resolve_manager_node_escalation(
                c.global_seat,
                &seat_resolve(&at_global[0], Some("Too late."), "v-global-rule")
            )
            .is_err()
    );
    let after = c
        .store
        .list_manager_node_escalations(c.pm, c.project)
        .unwrap()
        .into_iter()
        .find(|e| e.id == at_root.id)
        .unwrap();
    assert_eq!(after.state, ManagerNodeEscalationStateV1::Open);
    assert_eq!(after.above_project.unwrap().state, "retired");

    // A fresh chain: the source keeps its seat but takes a new grant.
    let c = chain();
    let at_root = escalate_to_root(&c, "g");
    let hop = c
        .store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &at_root, "g-root"))
        .unwrap()
        .above_project
        .unwrap();
    let at_global = c
        .store
        .tier_escalations_for_seat(c.global_seat)
        .unwrap()
        .unwrap();
    let mut regrant = area_request(
        &c.store,
        c.project,
        &c.parent,
        ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![c.epics[0]],
        },
    );
    regrant.node_id = Some(c.leaf.id);
    regrant.expected_node_grant_version = c.leaf.grant_version;
    regrant.seat_root_session_id = c.leaf.seat_root_session_id;
    regrant.grant.allowance.max_active_sessions -= 1;
    regrant.policy.max_active_sessions -= 1;
    let regranted = c.store.appoint_area_node(&regrant).unwrap();
    assert_ne!(regranted.grant_version, c.leaf.grant_version);
    assert_eq!(
        code(
            c.store
                .resolve_manager_node_escalation(
                    c.global_seat,
                    &seat_resolve(&at_global[0], None, "g-global-forward")
                )
                .unwrap_err()
        ),
        "manager_node_escalation_stale_source"
    );
    assert_eq!(hop_events(&c.store, hop.hop_id), ["opened", "retired"]);
}

/// With S3 (#1237), revoking a node re-parents its operator-granted children
/// under new grant versions. A re-parented child's queued mail and open hops
/// are retired like any grant change; the root forwards again and the chain
/// follows the new parent (here: the child became a root, so the operator).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_reparented_childs_mail_and_hops_retire_and_routing_follows_the_new_chain() {
    let c = chain();
    let queued = c
        .store
        .tier_report_up(c.pm, &report("for global", "p-up"))
        .unwrap();
    let at_root = escalate_to_root(&c, "p");
    let hop = c
        .store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &at_root, "p-root"))
        .unwrap()
        .above_project
        .unwrap();
    let (version, epoch) = grant_of(&c.store, c.pinnacle);
    c.store
        .revoke_portfolio_node(&RevokePortfolioNodeRequestV1 {
            node_id: c.pinnacle,
            expected_grant_version: version,
            expected_authority_epoch: epoch,
            idempotency_key: "p-revoke-pinnacle".into(),
        })
        .unwrap();
    let moved = c.store.get_portfolio_node(c.global).unwrap().unwrap();
    assert_eq!(moved.state, "active");
    assert_eq!(moved.parent_node_id, None, "re-parented to a root");
    assert_eq!(message_state(&c.store, queued.message_id), "retired");
    assert_eq!(hop_events(&c.store, hop.hop_id), ["opened", "retired"]);
    let back = c
        .store
        .list_manager_node_escalations(c.pm, c.project)
        .unwrap()
        .into_iter()
        .find(|e| e.id == at_root.id)
        .unwrap();
    let again = c
        .store
        .resolve_manager_node_escalation(c.pm, &root_forward(&c, &back, "p-root-again"))
        .unwrap()
        .above_project
        .unwrap();
    assert_eq!(again.target_ref, format!("portfolio:{}", c.global));
    assert_eq!(again.target_grant_version, Some(moved.grant.grant_version));
    let view = c
        .store
        .tier_escalations_for_seat(c.global_seat)
        .unwrap()
        .unwrap();
    let top = c
        .store
        .resolve_manager_node_escalation(
            c.global_seat,
            &seat_resolve(&view[0], None, "p-global-forward"),
        )
        .unwrap()
        .above_project
        .unwrap();
    assert_eq!(top.target_ref, OPERATOR_REF);
    let up = c
        .store
        .tier_report_up(c.global_seat, &report("root now", "p-up-2"))
        .unwrap();
    assert_eq!(up.target_ref, OPERATOR_REF);
}

/// #1293: a session holding both the PM seat and the pinnacle seat forwards
/// its local escalation above the root (to the separate global seat); its
/// identical retry replays the locally recorded result.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_dual_seat_pm_and_pinnacle_retry_replays_its_local_forward() {
    let c = chain();
    let view = c.store.get_portfolio_node(c.pinnacle).unwrap().unwrap();
    c.store
        .configure_portfolio_node(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: Some(c.pinnacle),
                parent_node_id: None,
                adopt_node_ids: vec![],
                expected_parent_grant_version: None,
                tier_label: "pinnacle".into(),
                seat_session_id: c.pm,
                project_ids: view.grant.project_ids.clone(),
                allowed_launches: view.grant.allowed_launches.clone(),
                policy: view.grant.project_policy.clone(),
                child_policy: view.child_policy.clone(),
                max_direct_reports: view.max_direct_reports,
                expected_node_grant_version: view.grant.grant_version,
                expected_authority_epoch: view.authority_epoch,
                idempotency_key: "dual-seat".into(),
            },
            PortfolioGrantor::Operator,
            "operator:test",
        )
        .unwrap();
    assert_eq!(
        c.store.tier_portfolio_ref(c.pm).unwrap(),
        Some(format!("portfolio:{}", c.pinnacle))
    );
    let at_root = escalate_to_root(&c, "dual");
    let forward = root_forward(&c, &at_root, "dual-root-forward");
    let first = c
        .store
        .resolve_manager_node_escalation(c.pm, &forward)
        .unwrap()
        .above_project
        .unwrap();
    assert_eq!(first.target_ref, format!("portfolio:{}", c.global));
    let retry = c
        .store
        .resolve_manager_node_escalation(c.pm, &forward)
        .unwrap()
        .above_project
        .unwrap();
    assert_eq!(retry.hop_id, first.hop_id);
    // A changed request under the used key still conflicts.
    let mut changed = forward.clone();
    changed.ruling = Some("Local ruling".into());
    assert_eq!(
        code(
            c.store
                .resolve_manager_node_escalation(c.pm, &changed)
                .unwrap_err()
        ),
        "manager_node_idempotency_conflict"
    );
}

/// #1294: a claim whose continuation is gone with no recorded outcome is
/// settled `uncertain` once it is older than the cutoff; an in-flight or
/// recorded claim (`exclude`) and a recent one are left alone.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_stale_claim_without_an_outcome_settles_uncertain() {
    let c = chain();
    let id = c
        .store
        .tier_send_down(
            c.pinnacle_seat,
            &send(
                ManagerNodeRefV1::Project {
                    project_id: c.project,
                },
                "stale-claim",
            ),
        )
        .unwrap()
        .message_id;
    assert!(claim_for(&c, &[id], c.pm));
    let past = Utc::now() - chrono::Duration::hours(1);
    let future = Utc::now() + chrono::Duration::seconds(5);
    assert_eq!(
        c.store
            .settle_stale_claimed_tier_messages(&[], past)
            .unwrap(),
        0
    );
    assert_eq!(
        c.store
            .settle_stale_claimed_tier_messages(&[id], future)
            .unwrap(),
        0
    );
    assert_eq!(message_state(&c.store, id), "claimed");
    assert_eq!(
        c.store
            .settle_stale_claimed_tier_messages(&[], future)
            .unwrap(),
        1
    );
    assert_eq!(message_state(&c.store, id), "uncertain");
    assert_eq!(
        settle_reason(&c.store, id).as_deref(),
        Some(TIER_SETTLEMENT_LOST_REASON)
    );
    assert!(!job_enabled(&c.store, id));
    assert_eq!(
        c.store.tier_message_state(id).unwrap().as_deref(),
        Some("uncertain")
    );
}

/// #1295: 300 unrelated newer failures never hide a seat's own failed mail:
/// the seat filter runs before the limit for the recipient and the sender,
/// and the operator pages through every row with a cursor.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_seats_failed_mail_stays_visible_past_unrelated_failures() {
    let c = chain();
    let target = ManagerNodeRefV1::Project {
        project_id: c.project,
    };
    let own = c
        .store
        .tier_send_down(c.pinnacle_seat, &send(target.clone(), "own"))
        .unwrap()
        .message_id;
    assert!(claim_for(&c, &[own], c.pm));
    c.store
        .settle_tier_messages(&[own], TierMailSettlement::Failed, Some("own failure"))
        .unwrap();
    let template = c
        .store
        .tier_send_down(c.pinnacle_seat, &send(target, "template"))
        .unwrap()
        .message_id
        .to_string();
    for index in 0..300 {
        c.store
            .conn
            .execute(
                "INSERT INTO manager_tier_messages(id,direction,kind,source_ref,target_ref,project_id,source_session_id,target_session_id,source_grant_version,target_grant_version,body,body_digest,idempotency_key,state,settle_reason,created_at,updated_at)
                 SELECT ?1,direction,kind,source_ref,target_ref,project_id,?2,?3,source_grant_version,target_grant_version,body,body_digest,?4,'failed','unrelated failure',created_at,'2099-01-01T00:00:00.000000000Z'
                 FROM manager_tier_messages WHERE id=?5",
                params![
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    format!("unrelated-{index}"),
                    template
                ],
            )
            .unwrap();
    }
    let received = c.store.undelivered_tier_mail_for_session(c.pm).unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].message_id, own);
    assert_eq!(received[0].role, "recipient");
    let inbox = c
        .store
        .manager_inbox(c.pm, &AgentManagerInboxRequestV1::default())
        .unwrap();
    assert_eq!(inbox.undelivered_tier_mail.len(), 1);
    assert_eq!(inbox.undelivered_tier_mail[0].message_id, own);
    assert!(!inbox.more_undelivered_tier_mail);
    let sent = c
        .store
        .undelivered_tier_mail_for_session(c.pinnacle_seat)
        .unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].message_id, own);
    assert_eq!(sent[0].role, "sender");

    // The operator pages through all 301 rows, newest first.
    let first = c.store.list_operator_escalations(false).unwrap();
    assert_eq!(first.undelivered.len(), OPERATOR_QUEUE_LIMIT);
    let cursor = first.next_undelivered_after.clone().expect("a second page");
    let second = c
        .store
        .list_operator_escalations_page(false, Some(&cursor))
        .unwrap();
    assert_eq!(second.undelivered.len(), 301 - OPERATOR_QUEUE_LIMIT);
    assert_eq!(second.next_undelivered_after, None);
    assert_eq!(second.undelivered.last().unwrap().message_id, own);
    let mut seen: Vec<Uuid> = first
        .undelivered
        .iter()
        .chain(&second.undelivered)
        .map(|row| row.message_id)
        .collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 301, "every row is listed exactly once");
    assert_eq!(
        code(
            c.store
                .list_operator_escalations_page(false, Some("not-a-cursor"))
                .unwrap_err()
        ),
        "manager_tier_invalid_request"
    );
}

/// #1308: 256 excluded (in-flight or recorded) claims older than one orphan
/// never starve it: the exclusion applies before the batch limit, so the
/// orphan settles `uncertain` within one sweep and the excluded claims stay.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn excluded_claims_never_starve_an_orphan_claim() {
    let c = chain();
    let template = c
        .store
        .tier_send_down(
            c.pinnacle_seat,
            &send(
                ManagerNodeRefV1::Project {
                    project_id: c.project,
                },
                "starve-template",
            ),
        )
        .unwrap()
        .message_id
        .to_string();
    let insert_claimed = |key: String, at: &str| {
        let id = Uuid::new_v4();
        c.store
            .conn
            .execute(
                "INSERT INTO manager_tier_messages(id,direction,kind,source_ref,target_ref,project_id,source_session_id,target_session_id,source_grant_version,target_grant_version,body,body_digest,idempotency_key,state,settle_reason,created_at,updated_at)
                 SELECT ?1,direction,kind,source_ref,target_ref,project_id,source_session_id,target_session_id,source_grant_version,target_grant_version,body,body_digest,?2,'claimed',NULL,created_at,?3
                 FROM manager_tier_messages WHERE id=?4",
                params![id.to_string(), key, at, template],
            )
            .unwrap();
        id
    };
    // 256 excluded claims, all older than the orphan, fill a whole batch.
    let excluded: Vec<Uuid> = (0..256)
        .map(|index| {
            insert_claimed(
                format!("starve-excluded-{index}"),
                &format!("2020-01-01T00:00:{:02}.{:09}Z", index / 60, index),
            )
        })
        .collect();
    let orphan = insert_claimed("starve-orphan".into(), "2021-01-01T00:00:00.000000000Z");
    assert_eq!(
        c.store
            .settle_stale_claimed_tier_messages(&excluded, Utc::now())
            .unwrap(),
        1
    );
    assert_eq!(message_state(&c.store, orphan), "uncertain");
    assert_eq!(
        settle_reason(&c.store, orphan).as_deref(),
        Some(TIER_SETTLEMENT_LOST_REASON)
    );
    assert!(
        excluded
            .iter()
            .all(|id| message_state(&c.store, *id) == "claimed"),
        "excluded claims are untouched"
    );
    // More than one batch of orphans settles in the same sweep.
    let orphans: Vec<Uuid> = (0..300)
        .map(|index| {
            insert_claimed(
                format!("starve-many-{index}"),
                &format!("2022-01-01T00:00:{:02}.{:09}Z", index / 60, index),
            )
        })
        .collect();
    assert_eq!(
        c.store
            .settle_stale_claimed_tier_messages(&excluded, Utc::now())
            .unwrap(),
        300
    );
    assert!(
        orphans
            .iter()
            .all(|id| message_state(&c.store, *id) == "uncertain")
    );
}
