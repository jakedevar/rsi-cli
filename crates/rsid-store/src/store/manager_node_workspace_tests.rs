//! #1240 (fractal manager hierarchy S6): one snapshot of any manager node,
//! pinned on pinnacle -> {global 1, global 2, project D} -> projects ->
//! project root (PM) -> parent area -> leaf area.

use super::*;
use crate::store::manager_nodes::AreaNode;
use crate::store::manager_nodes::tests::{area_fixture, area_request};
use crate::store::portfolio_nodes::PortfolioGrantor;
use crate::test_support::test_session;
use rsi_common::fleet::{FLEET_WINDOWS, FleetUsage};
use rsi_common::harness_manager::{
    AgentManagerEscalateRequestV1, AgentManagerResolveEscalationRequestV1,
    ManagerNodeEscalationRouteV1,
};
use rsi_common::harness_manager_v2::{
    ManagerCapabilityV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerPolicyV2,
};
use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;
use rsi_common::types::{Project, SessionKind, SessionProvider, SessionStatus};
use std::path::PathBuf;

fn session(
    store: &Store,
    project: Option<Uuid>,
    parent: Option<Uuid>,
    status: SessionStatus,
) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/node-workspace"));
    row.project_id = project;
    row.parent_id = parent;
    row.session_kind = SessionKind::Standard;
    row.status = status;
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

/// Each finite allowance a little under a quarter of its parent's per level,
/// so siblings fit under their parent (#1302); capabilities stay equal.
fn tier_policy(level: u16) -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![ManagerCapabilityV2::IssueCoordinate],
        max_created_containers: (64 >> (2 * level)) - 1,
        max_created_sessions: (128 >> (2 * level)) - 1,
        max_active_sessions: (64 >> (2 * level)) - 1,
        ..ManagerPolicyV2::default()
    }
}

fn portfolio(
    store: &Store,
    parent: Option<Uuid>,
    label: &str,
    seat: Uuid,
    projects: &[Uuid],
) -> Uuid {
    let parent_view = parent.map(|id| store.get_portfolio_node(id).unwrap().unwrap());
    // The area fixture's active cap is 100; these tiers lower it to 63 then 15.
    store
        .configure_portfolio_node_confirmed(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: parent,
                adopt_node_ids: vec![],
                expected_parent_grant_version: parent_view
                    .as_ref()
                    .map(|view| view.grant.grant_version),
                tier_label: label.into(),
                seat_session_id: seat,
                project_ids: projects.to_vec(),
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: None,
                }],
                policy: tier_policy(u16::from(parent.is_some())),
                child_policy: None,
                max_direct_reports: if parent.is_some() { 4 } else { 5 },
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: format!("node-{label}-{seat}"),
            },
            PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .unwrap()
        .node_id
}

struct Tree {
    store: Store,
    /// Project A holds the PM, the areas and the Epics.
    a: Uuid,
    b: Uuid,
    c: Uuid,
    d: Uuid,
    pm: Uuid,
    root: AreaNode,
    parent: AreaNode,
    leaf: AreaNode,
    epics: Vec<Uuid>,
    pinnacle: Uuid,
    pinnacle_seat: Uuid,
    global1: Uuid,
    global1_seat: Uuid,
    global2: Uuid,
}

fn tree() -> Tree {
    let (store, a, root, group, epics) = area_fixture(true);
    let parent = store
        .appoint_area_node(&area_request(
            &store,
            a,
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
            a,
            &parent,
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![epics[0]],
            },
        ))
        .unwrap();
    let (b, c, d) = (
        project(&store, "B"),
        project(&store, "C"),
        project(&store, "D"),
    );
    let pinnacle_seat = session(&store, None, None, SessionStatus::Completed);
    let pinnacle = portfolio(&store, None, "pinnacle", pinnacle_seat, &[a, b, c, d]);
    let global1_seat = session(&store, None, None, SessionStatus::Completed);
    let global1 = portfolio(&store, Some(pinnacle), "global", global1_seat, &[a, b]);
    let global2_seat = session(&store, None, None, SessionStatus::Completed);
    let global2 = portfolio(&store, Some(pinnacle), "global", global2_seat, &[c]);
    let pm = root.seat_root_session_id;
    assert_eq!(store.global_live_manager(a).unwrap(), Some(pm));
    Tree {
        store,
        a,
        b,
        c,
        d,
        pm,
        root,
        parent,
        leaf,
        epics,
        pinnacle,
        pinnacle_seat,
        global1,
        global1_seat,
        global2,
    }
}

fn snapshot(store: &Store, node: ManagerNodeRefV1, span: ProjectSpan) -> NodeWorkspaceRaw {
    store
        .manager_node_workspace_raw(node, span, Utc::now(), true)
        .unwrap()
}

fn children(raw: &NodeWorkspaceRaw) -> Vec<ManagerNodeRefV1> {
    raw.children.iter().map(|child| child.node).collect()
}

fn projects(raw: &NodeWorkspaceRaw) -> Vec<Uuid> {
    raw.project_rows.iter().map(|row| row.project_id).collect()
}

fn portfolio_ref(node_id: Uuid) -> ManagerNodeRefV1 {
    ManagerNodeRefV1::Portfolio { node_id }
}

fn project_ref(project_id: Uuid) -> ManagerNodeRefV1 {
    ManagerNodeRefV1::Project { project_id }
}

fn area_ref(node_id: Uuid) -> ManagerNodeRefV1 {
    ManagerNodeRefV1::Area { node_id }
}

/// Acceptance: the snapshots of a pinnacle, a global, a project and an area
/// node list exactly that node's children and coverage, and `parent_of`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn each_tier_snapshot_lists_exactly_its_children_and_coverage() {
    let t = tree();
    let s = &t.store;

    let pinnacle = snapshot(s, portfolio_ref(t.pinnacle), ProjectSpan::Coverage);
    assert_eq!(pinnacle.label, "pinnacle");
    assert_eq!(pinnacle.state, "active");
    assert_eq!(pinnacle.parent, None);
    assert_eq!(pinnacle.seat_session_id, Some(t.pinnacle_seat));
    assert_eq!(pinnacle.grantor.as_deref(), Some("operator"));
    assert_eq!(
        children(&pinnacle),
        vec![
            portfolio_ref(t.global1),
            portfolio_ref(t.global2),
            project_ref(t.d)
        ]
    );
    assert_eq!(projects(&pinnacle), vec![t.a, t.b, t.c, t.d]);
    // A child global is one digest: its coverage and summed counts, no rows.
    let digest = &pinnacle.children[0];
    assert_eq!(digest.label, "global");
    assert_eq!(digest.project_ids, vec![t.a, t.b]);
    assert_eq!(digest.seat_session_id, Some(t.global1_seat));
    let summed = sum_counts(pinnacle.project_rows.iter().take(2));
    assert_eq!(digest.counts, Some(summed));
    // The agent span keeps only the project the pinnacle manages directly.
    let direct = snapshot(s, portfolio_ref(t.pinnacle), ProjectSpan::Direct);
    assert_eq!(projects(&direct), vec![t.d]);
    assert_eq!(children(&direct), children(&pinnacle));

    let global = snapshot(s, portfolio_ref(t.global1), ProjectSpan::Coverage);
    assert_eq!(global.parent, Some(portfolio_ref(t.pinnacle)));
    assert_eq!(children(&global), vec![project_ref(t.a), project_ref(t.b)]);
    assert_eq!(projects(&global), vec![t.a, t.b]);
    let pm_digest = &global.children[0];
    assert_eq!(pm_digest.state, "active");
    assert_eq!(pm_digest.seat_session_id, Some(t.pm));
    assert_eq!(global.children[1].state, "vacant");
    assert_eq!(
        projects(&snapshot(s, portfolio_ref(t.global1), ProjectSpan::Direct)),
        vec![t.a, t.b]
    );
    let global2 = snapshot(s, portfolio_ref(t.global2), ProjectSpan::Coverage);
    assert_eq!(children(&global2), vec![project_ref(t.c)]);
    assert_eq!(projects(&global2), vec![t.c]);

    let project = snapshot(s, project_ref(t.a), ProjectSpan::Coverage);
    assert_eq!(project.parent, Some(portfolio_ref(t.global1)));
    assert_eq!(project.seat_session_id, Some(t.pm));
    assert_eq!(project.state, "active");
    assert_eq!(children(&project), vec![area_ref(t.parent.id)]);
    assert_eq!(projects(&project), vec![t.a]);
    // A project's root area is addressed as its project.
    assert_eq!(
        snapshot(s, area_ref(t.root.id), ProjectSpan::Coverage).node,
        project_ref(t.a)
    );

    let area = snapshot(s, area_ref(t.parent.id), ProjectSpan::Coverage);
    assert_eq!(area.parent, Some(project_ref(t.a)));
    assert_eq!(area.state, "active");
    assert_eq!(children(&area), vec![area_ref(t.leaf.id)]);
    assert_eq!(projects(&area), vec![t.a]);
    assert_eq!(
        area.area.as_ref().map(|grant| grant.grant_version),
        Some(t.parent.grant_version)
    );
    assert_eq!(area.children[0].counts, None);
    let leaf = snapshot(s, area_ref(t.leaf.id), ProjectSpan::Coverage);
    assert_eq!(leaf.parent, Some(area_ref(t.parent.id)));
    assert!(leaf.children.is_empty());

    for unknown in [
        portfolio_ref(Uuid::new_v4()),
        project_ref(Uuid::new_v4()),
        area_ref(Uuid::new_v4()),
    ] {
        let error = s
            .manager_node_workspace_raw(unknown, ProjectSpan::Coverage, Utc::now(), false)
            .err()
            .unwrap();
        assert!(
            matches!(&error, DaemonError::InvalidParam(code) if code == MANAGER_TIER_TARGET_UNKNOWN),
            "{error:?}"
        );
    }

    // Each seat resolves to its own node; a plain session to none.
    assert_eq!(
        s.manager_caller_node(t.pinnacle_seat).unwrap(),
        Some(portfolio_ref(t.pinnacle))
    );
    assert_eq!(s.manager_caller_node(t.pm).unwrap(), Some(project_ref(t.a)));
    let worker = session(s, Some(t.b), None, SessionStatus::Running);
    assert_eq!(s.manager_caller_node(worker).unwrap(), None);
}

fn invocation(store: &Store, project: Option<Uuid>, session: Option<Uuid>, minutes_ago: i64) {
    store
        .conn
        .execute(
            "INSERT INTO model_invocations (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,project_id,session_id,provider,model,created_at,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,estimated_cost_usd)
             VALUES (?1,'session','cli','foreground','paid','admitted','completed','test',?2,?3,'Claude','claude-opus-5-5',?4,10,20,30,40,0.5)",
            params![
                Uuid::new_v4().to_string(),
                project.map(|id| id.to_string()),
                session.map(|id| id.to_string()),
                (Utc::now() - chrono::Duration::minutes(minutes_ago))
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
}

fn add(total: &mut FleetUsage, usage: &FleetUsage) {
    total.invocations += usage.invocations;
    total.errors += usage.errors;
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.cost += usage.cost;
    total.unknown_usage += usage.unknown_usage;
}

/// #1232's operator totals, filtered to `coverage` by project group.
fn filtered(fleet: &rsi_common::fleet::FleetOverview, coverage: &[Uuid]) -> (u64, [FleetUsage; 3]) {
    let mut active = 0;
    let mut totals: [FleetUsage; 3] = Default::default();
    for group in &fleet.groups {
        if group.dimension == "project" && coverage.iter().any(|id| id.to_string() == group.key) {
            active += group.active;
            for (window, usage) in group.windows.iter().enumerate() {
                add(&mut totals[window], usage);
            }
        }
    }
    (active, totals)
}

/// Acceptance: every node's fleet rollup equals #1232's fleet totals filtered
/// to its coverage; an area keeps only the sessions under its Epics.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn fleet_rollups_equal_the_fleet_totals_filtered_to_each_coverage() {
    let t = tree();
    let s = &t.store;
    let leaf_worker = session(s, Some(t.a), Some(t.epics[0]), SessionStatus::Running);
    let other_epic_worker = session(s, Some(t.a), Some(t.epics[1]), SessionStatus::Running);
    let mut running = vec![leaf_worker, other_epic_worker];
    for (project, count) in [(t.a, 1), (t.b, 2), (t.c, 3), (t.d, 1)] {
        for _ in 0..count {
            running.push(session(s, Some(project), None, SessionStatus::Running));
        }
    }
    let unassigned = session(s, None, None, SessionStatus::Running);
    invocation(s, Some(t.a), Some(leaf_worker), 2);
    invocation(s, Some(t.a), Some(other_epic_worker), 30);
    invocation(s, Some(t.b), Some(running[3]), 2);
    invocation(s, Some(t.c), None, 120);
    invocation(s, Some(t.c), None, 2);
    invocation(s, Some(t.d), None, 2);
    invocation(s, None, Some(unassigned), 2);

    let now = Utc::now();
    let fleet = s.fleet_overview(now).unwrap();
    for (node, coverage) in [
        (portfolio_ref(t.pinnacle), vec![t.a, t.b, t.c, t.d]),
        (portfolio_ref(t.global1), vec![t.a, t.b]),
        (portfolio_ref(t.global2), vec![t.c]),
        (project_ref(t.a), vec![t.a]),
    ] {
        let rollup = s
            .manager_node_workspace_raw(node, ProjectSpan::Coverage, now, true)
            .unwrap()
            .fleet
            .unwrap();
        let (active, totals) = filtered(&fleet, &coverage);
        assert_eq!(rollup.active, active, "{node:?}");
        assert_eq!(rollup.totals, totals, "{node:?}");
        assert!(
            rollup
                .groups
                .iter()
                .all(|group| group.dimension != "project"
                    || coverage.iter().any(|id| id.to_string() == group.key))
        );
    }
    // The pinnacle covers every project row but not the unassigned one.
    let pinnacle = snapshot(s, portfolio_ref(t.pinnacle), ProjectSpan::Coverage)
        .fleet
        .unwrap();
    assert_eq!(pinnacle.active + 1, fleet.agents.len() as u64);
    assert_eq!(
        pinnacle.totals[2].invocations + 1,
        fleet.totals[2].invocations
    );
    // The leaf area covers Epic 0: its worker and that worker's usage.
    let leaf = snapshot(s, area_ref(t.leaf.id), ProjectSpan::Coverage)
        .fleet
        .unwrap();
    assert_eq!(leaf.active, 1);
    assert_eq!(leaf.totals[2].invocations, 1);
    assert_eq!(leaf.totals[0].invocations, 1);
    assert_eq!(FLEET_WINDOWS[0], 300);
    // The parent area's Group selects both Epics.
    let parent = snapshot(s, area_ref(t.parent.id), ProjectSpan::Coverage)
        .fleet
        .unwrap();
    assert_eq!(parent.active, 2);
    assert_eq!(parent.totals[2].invocations, 2);
    // The shim's read skips the rollup.
    assert!(
        s.manager_node_workspace_raw(portfolio_ref(t.global1), ProjectSpan::Coverage, now, false)
            .unwrap()
            .fleet
            .is_none()
    );
}

/// Escalations wait on exactly one node: the project root until it is
/// forwarded, then the covering global's hop; the pinnacle sees the count in
/// that global's digest.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn escalations_are_listed_at_the_node_they_wait_on() {
    let t = tree();
    let s = &t.store;
    let created = s
        .create_manager_node_escalation(
            t.leaf.seat_root_session_id,
            &AgentManagerEscalateRequestV1 {
                project_id: t.a,
                subject_id: Uuid::new_v4(),
                reason: "Two areas claim the release branch".into(),
                route: ManagerNodeEscalationRouteV1::Parent,
                expected_source_authority_epoch: t.leaf.authority_epoch,
                expected_source_grant_version: t.leaf.grant_version,
                expected_target_authority_epoch: t.parent.authority_epoch,
                expected_target_grant_version: t.parent.grant_version,
                expected_target_session_id: t.parent.seat_root_session_id,
                idempotency_key: "esc-create".into(),
            },
        )
        .unwrap();
    let parent = snapshot(s, area_ref(t.parent.id), ProjectSpan::Coverage);
    assert_eq!(parent.escalations.len(), 1);
    assert_eq!(parent.escalations[0].escalation_id, created.id);
    assert_eq!(parent.escalations[0].hop, None);
    let project = snapshot(s, project_ref(t.a), ProjectSpan::Coverage);
    assert_eq!(project.children[0].pending_escalations, 1);

    let at_root = s
        .resolve_manager_node_escalation(
            t.parent.seat_root_session_id,
            &AgentManagerResolveEscalationRequestV1 {
                escalation_id: created.id,
                expected_version: created.version,
                expected_target_authority_epoch: t.parent.authority_epoch,
                expected_target_grant_version: t.parent.grant_version,
                expected_target_session_id: t.parent.seat_root_session_id,
                ruling: None,
                idempotency_key: "esc-parent-forward".into(),
            },
        )
        .unwrap();
    assert_eq!(
        snapshot(s, project_ref(t.a), ProjectSpan::Coverage)
            .escalations
            .len(),
        1
    );
    s.resolve_manager_node_escalation(
        t.pm,
        &AgentManagerResolveEscalationRequestV1 {
            escalation_id: at_root.id,
            expected_version: at_root.version,
            expected_target_authority_epoch: t.root.authority_epoch,
            expected_target_grant_version: t.root.grant_version,
            expected_target_session_id: t.pm,
            ruling: None,
            idempotency_key: "esc-root-forward".into(),
        },
    )
    .unwrap();
    // Held above the project root: the global lists the hop; the project
    // and its parent area no longer do.
    assert!(
        snapshot(s, project_ref(t.a), ProjectSpan::Coverage)
            .escalations
            .is_empty()
    );
    let global = snapshot(s, portfolio_ref(t.global1), ProjectSpan::Coverage);
    assert_eq!(global.escalations.len(), 1);
    assert_eq!(global.escalations[0].escalation_id, created.id);
    assert_eq!(global.escalations[0].hop, Some(1));
    assert_eq!(global.children[0].pending_escalations, 0);
    let pinnacle = snapshot(s, portfolio_ref(t.pinnacle), ProjectSpan::Direct);
    assert!(pinnacle.escalations.is_empty());
    assert_eq!(pinnacle.children[0].pending_escalations, 1);
    assert_eq!(pinnacle.children[1].pending_escalations, 0);
    let _ = (t.b, t.c, t.global2);
}

/// #1328: an Epic-scoped area's own overview (`ProjectSpan::Direct`) carries
/// no whole-project row, so work under a sibling Epic outside its grant moves
/// nothing it sees; the operator workspace still shows the containing
/// project.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn area_overview_excludes_sibling_epic_work() {
    let t = tree();
    let s = &t.store;
    session(s, Some(t.a), Some(t.epics[0]), SessionStatus::Running);
    let before = snapshot(s, area_ref(t.leaf.id), ProjectSpan::Direct);
    let sibling = session(
        s,
        Some(t.a),
        Some(t.epics[1]),
        SessionStatus::WaitingApproval,
    );
    invocation(s, Some(t.a), Some(sibling), 2);
    let after = snapshot(s, area_ref(t.leaf.id), ProjectSpan::Direct);
    for direct in [&before, &after] {
        assert_eq!(direct.node, area_ref(t.leaf.id));
        assert_eq!(projects(direct), Vec::<Uuid>::new());
        let fleet = direct.fleet.as_ref().unwrap();
        assert_eq!(fleet.active, 1);
        assert_eq!(fleet.totals[2].invocations, 0);
        assert_eq!(direct.area.as_ref().map(|area| area.project_id), Some(t.a));
    }
    // The sibling Epic's work is real: the project and the parent area
    // (whose Group selects both Epics) see it.
    let project = snapshot(s, project_ref(t.a), ProjectSpan::Coverage);
    assert_eq!(project.project_rows[0].waiting_approval_sessions, 1);
    let parent = snapshot(s, area_ref(t.parent.id), ProjectSpan::Direct);
    assert_eq!(parent.fleet.unwrap().active, 2);
    // The operator workspace of the area keeps the containing project row.
    let operator = snapshot(s, area_ref(t.leaf.id), ProjectSpan::Coverage);
    assert_eq!(projects(&operator), vec![t.a]);
}

/// #1329: an area's coverage is every session under its Epics, not the first
/// 4096; 4097 completed workers with one recent invocation each count fully
/// and the rollup is not truncated. A clipped tree marks it truncated.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn area_fleet_counts_every_worker_and_flags_a_clipped_tree() {
    const WORKERS: usize = 4097;
    let t = tree();
    let s = &t.store;
    s.conn.execute_batch("BEGIN").unwrap();
    for _ in 0..WORKERS {
        let worker = session(s, Some(t.a), Some(t.epics[0]), SessionStatus::Completed);
        invocation(s, Some(t.a), Some(worker), 2);
    }
    s.conn.execute_batch("COMMIT").unwrap();
    let fleet = snapshot(s, area_ref(t.leaf.id), ProjectSpan::Direct)
        .fleet
        .unwrap();
    assert_eq!(fleet.totals[2].invocations, WORKERS as u64);
    assert!(!fleet.usage_truncated);
    assert!(!fleet.agents_truncated);

    let selector = ManagerNodeSelectorV1::Selected {
        group_ids: vec![],
        epic_ids: vec![t.epics[0]],
    };
    let (scope, clipped) = s.area_fleet_scope(t.a, Some(&selector), 16).unwrap();
    assert!(clipped);
    assert!(matches!(&scope, FleetScope::Sessions(ids) if ids.contains(&t.epics[0])));
    let (_, clipped) = s
        .area_fleet_scope(t.a, Some(&selector), WORKERS + 1)
        .unwrap();
    assert!(!clipped);
}
