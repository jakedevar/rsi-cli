//! #1240: the manager console on any node (pinnacle, global, project, area).

use chrono::Utc;
use crossterm::event::KeyCode;
use rsi_common::fleet::{FleetGroup, FleetUsage};
use rsi_common::global_manager::GlobalIssueCountsV1;
use rsi_common::manager_node_workspace::{
    ManagerNodeAreaGrantV1, ManagerNodeChildV1, ManagerNodeCountsV1, ManagerNodeFleetV1,
    ManagerNodePendingEscalationV1, ManagerNodeWorkspaceV1,
};
use rsi_common::manager_nodes::ManagerNodeSelectorV1;
use rsi_common::manager_tier_routing::ManagerNodeRefV1;
use rsi_common::types::{Project, SessionStatus};
use uuid::Uuid;

use super::tests::{Fixture, fake_daemon, grant, key, pm, policy, portfolio, project, seat};
use super::*;

pub(crate) const PINNACLE: Uuid = Uuid::from_u128(0xa124_0001_0000_0000_0000_0000_0000_0000);
pub(crate) const GLOBAL_1: Uuid = Uuid::from_u128(0xb124_0002_0000_0000_0000_0000_0000_0000);
pub(crate) const GLOBAL_2: Uuid = Uuid::from_u128(0xc124_0003_0000_0000_0000_0000_0000_0000);
pub(crate) const AREA: Uuid = Uuid::from_u128(0xd124_0004_0000_0000_0000_0000_0000_0000);
pub(crate) const PINNACLE_SEAT: Uuid = Uuid::from_u128(0xe124_0010_0000_0000_0000_0000_0000_0000);
pub(crate) const AREA_SEAT: Uuid = Uuid::from_u128(0xf124_0011_0000_0000_0000_0000_0000_0000);

/// Two more projects for the pinnacle: `billing` under global 2 and
/// `notes`, which the pinnacle manages directly.
pub(crate) fn pinnacle_projects() -> (Project, Project) {
    let mut billing = project("billing");
    billing.id = Uuid::from_u128(0x1240_0000_0000_0000_0000_0000_0000_0020);
    let mut notes = project("notes");
    notes.id = Uuid::from_u128(0x1240_0000_0000_0000_0000_0000_0000_0021);
    (billing, notes)
}

fn counts(open: i64, running: i64) -> ManagerNodeCountsV1 {
    ManagerNodeCountsV1 {
        issues: GlobalIssueCountsV1 {
            open,
            in_progress: 2,
            open_operator_requests: 1,
        },
        running_sessions: running,
        waiting_approval_sessions: 1,
        pending_questions: 0,
        pending_approvals: 1,
    }
}

fn usage(invocations: u64, tokens: u64, cost: f64) -> FleetUsage {
    FleetUsage {
        invocations,
        errors: 0,
        input: tokens,
        output: tokens / 2,
        cache_read: 0,
        cache_write: 0,
        cost,
        unknown_usage: 0,
    }
}

fn fleet(active: u64) -> ManagerNodeFleetV1 {
    ManagerNodeFleetV1 {
        as_of: Utc::now(),
        active,
        totals: [
            usage(4, 2000, 0.05),
            usage(40, 24000, 0.6),
            usage(410, 260_000, 6.5),
        ],
        groups: vec![FleetGroup {
            dimension: "provider".into(),
            key: "Claude".into(),
            label: "Claude".into(),
            active,
            windows: Default::default(),
        }],
        agents_truncated: false,
        usage_truncated: false,
    }
}

fn child(
    node: ManagerNodeRefV1,
    label: &str,
    seat_session: Option<GlobalSeatSessionV1>,
    project_ids: Vec<Uuid>,
    child_counts: Option<ManagerNodeCountsV1>,
) -> ManagerNodeChildV1 {
    ManagerNodeChildV1 {
        node,
        label: label.into(),
        state: if seat_session.is_some() {
            "active".into()
        } else {
            "vacant".into()
        },
        grantor: Some("operator".into()),
        grant_version: 2,
        seat: seat_session,
        project_ids,
        counts: child_counts,
        pending_escalations: 0,
    }
}

/// pinnacle [rsi, dictate-agent, billing, notes] -> global 1 [rsi,
/// dictate-agent] (the fixture's global seat), global 2 [billing] (vacant);
/// `notes` is the pinnacle's own project.
pub(crate) fn pinnacle_node(f: &Fixture) -> ManagerNodeWorkspaceV1 {
    let (billing, notes) = pinnacle_projects();
    let mut projects = f.snapshot.projects.clone();
    projects.push(portfolio(&billing, None, None, false));
    projects.push(portfolio(
        &notes,
        Some(pm(Uuid::from_u128(0x1240_0030), SessionStatus::Completed)),
        Some(policy(false, false)),
        false,
    ));
    let mut global1 = child(
        ManagerNodeRefV1::Portfolio { node_id: GLOBAL_1 },
        "global",
        f.snapshot.seat.clone(),
        vec![f.a.id, f.b.id],
        Some(counts(24, 10)),
    );
    global1.pending_escalations = 1;
    ManagerNodeWorkspaceV1 {
        node: ManagerNodeRefV1::Portfolio { node_id: PINNACLE },
        label: "pinnacle".into(),
        state: "active".into(),
        parent: None,
        grant: Some(grant(
            PINNACLE_SEAT,
            &[f.a.id, f.b.id, billing.id, notes.id],
            "active",
        )),
        grantor: Some("operator".into()),
        max_direct_reports: Some(5),
        area: None,
        seat: Some(seat(PINNACLE_SEAT, None, SessionStatus::Completed)),
        children: vec![
            global1,
            child(
                ManagerNodeRefV1::Portfolio { node_id: GLOBAL_2 },
                "global",
                None,
                vec![billing.id],
                Some(counts(12, 5)),
            ),
            child(
                ManagerNodeRefV1::Project {
                    project_id: notes.id,
                },
                "notes",
                Some(seat(
                    Uuid::from_u128(0x1240_0030),
                    Some(notes.id),
                    SessionStatus::Completed,
                )),
                vec![notes.id],
                Some(counts(12, 5)),
            ),
        ],
        children_truncated: false,
        projects,
        missing_project_ids: vec![],
        escalations: vec![ManagerNodePendingEscalationV1 {
            escalation_id: Uuid::from_u128(0x1240_0040),
            project_id: notes.id,
            subject_id: Uuid::from_u128(0x1240_0041),
            reason: "Two globals claim the release train".into(),
            hop: Some(2),
            created_at: Utc::now(),
        }],
        escalations_truncated: false,
        fleet: fleet(20),
    }
}

/// Global 1 under the pinnacle: the fixture's grant, seat and projects.
pub(crate) fn global_node(f: &Fixture) -> ManagerNodeWorkspaceV1 {
    let projects = f.snapshot.projects.clone();
    ManagerNodeWorkspaceV1 {
        node: ManagerNodeRefV1::Portfolio { node_id: GLOBAL_1 },
        label: "global".into(),
        state: "active".into(),
        parent: Some(ManagerNodeRefV1::Portfolio { node_id: PINNACLE }),
        grant: f.snapshot.grant.clone(),
        grantor: Some("operator".into()),
        max_direct_reports: Some(4),
        area: None,
        seat: f.snapshot.seat.clone(),
        children: vec![
            child(
                ManagerNodeRefV1::Project { project_id: f.a.id },
                &f.a.name,
                Some(seat(f.pm_id, Some(f.a.id), SessionStatus::Running)),
                vec![f.a.id],
                Some(counts(12, 5)),
            ),
            child(
                ManagerNodeRefV1::Project { project_id: f.b.id },
                &f.b.name,
                None,
                vec![f.b.id],
                Some(counts(12, 5)),
            ),
        ],
        children_truncated: false,
        projects,
        missing_project_ids: vec![],
        escalations: vec![],
        escalations_truncated: false,
        fleet: fleet(10),
    }
}

/// The project node of `rsi` (its PM seat) with one child area.
pub(crate) fn project_node(f: &Fixture) -> ManagerNodeWorkspaceV1 {
    ManagerNodeWorkspaceV1 {
        node: ManagerNodeRefV1::Project { project_id: f.a.id },
        label: f.a.name.clone(),
        state: "active".into(),
        parent: Some(ManagerNodeRefV1::Portfolio { node_id: GLOBAL_1 }),
        grant: None,
        grantor: None,
        max_direct_reports: None,
        area: None,
        seat: Some(seat(f.pm_id, Some(f.a.id), SessionStatus::Running)),
        children: vec![child(
            ManagerNodeRefV1::Area { node_id: AREA },
            "area",
            Some(seat(AREA_SEAT, Some(f.a.id), SessionStatus::Completed)),
            vec![f.a.id],
            None,
        )],
        children_truncated: false,
        projects: vec![f.snapshot.projects[0].clone()],
        missing_project_ids: vec![],
        escalations: vec![],
        escalations_truncated: false,
        fleet: fleet(5),
    }
}

/// The child area of `rsi`'s project node.
pub(crate) fn area_node(f: &Fixture) -> ManagerNodeWorkspaceV1 {
    ManagerNodeWorkspaceV1 {
        node: ManagerNodeRefV1::Area { node_id: AREA },
        label: "area".into(),
        state: "active".into(),
        parent: Some(ManagerNodeRefV1::Project { project_id: f.a.id }),
        grant: None,
        grantor: None,
        max_direct_reports: Some(3),
        area: Some(ManagerNodeAreaGrantV1 {
            project_id: f.a.id,
            active: true,
            grant_version: 4,
            authority_epoch: 1,
            selector: Some(ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![Uuid::from_u128(0x1240_0050)],
            }),
            grant: None,
        }),
        seat: Some(seat(AREA_SEAT, Some(f.a.id), SessionStatus::Completed)),
        children: vec![],
        children_truncated: false,
        projects: vec![f.snapshot.projects[0].clone()],
        missing_project_ids: vec![],
        escalations: vec![],
        escalations_truncated: false,
        fleet: fleet(1),
    }
}

pub(crate) fn node_state(
    node: ManagerNodeWorkspaceV1,
    at: chrono::DateTime<Utc>,
) -> GlobalManagerWorkspaceState {
    let mut state = GlobalManagerWorkspaceState {
        target: Some(node.node),
        ..GlobalManagerWorkspaceState::default()
    };
    state.install_node(node, at);
    state
}

fn shape(state: &GlobalManagerWorkspaceState) -> Vec<(SeatLevel, usize, String, SeatHealth)> {
    state
        .seats()
        .into_iter()
        .map(|seat| (seat.level, seat.depth, seat.label, seat.health))
        .collect()
}

#[test]
fn a_pinnacle_console_lists_child_nodes_with_their_projects_then_its_own() {
    let f = super::tests::fixture();
    let state = node_state(pinnacle_node(&f), Utc::now());
    let g1 = format!("global {}", &GLOBAL_1.to_string()[..8]);
    let g2 = format!("global {}", &GLOBAL_2.to_string()[..8]);
    assert_eq!(
        shape(&state),
        vec![
            (
                SeatLevel::Portfolio,
                0,
                "pinnacle manager".into(),
                SeatHealth::Idle
            ),
            (SeatLevel::Portfolio, 1, g1, SeatHealth::Idle),
            (SeatLevel::Project, 2, "rsi".into(), SeatHealth::Active),
            (
                SeatLevel::Project,
                2,
                "dictate-agent".into(),
                SeatHealth::Missing
            ),
            (SeatLevel::Portfolio, 1, g2, SeatHealth::Missing),
            (SeatLevel::Project, 2, "billing".into(), SeatHealth::Missing),
            (SeatLevel::Project, 1, "notes".into(), SeatHealth::Idle),
        ]
    );
    // A child digest carries its summed counts and its seat.
    let global = state.seat(WorkspaceRow::Child(0)).unwrap();
    assert_eq!(global.issues, Some((24, 2, 1)));
    assert_eq!(global.session_id, Some(f.seat_id));
    assert_eq!(
        seat_summary(&global)[0],
        format!(
            "GLOBAL SEAT · global {} · IDLE · claude-opus-5-5 · session {} · ctx 37% · $4.20 · updated {}",
            &GLOBAL_1.to_string()[..8],
            &f.seat_id.to_string()[..8],
            global.updated_at.unwrap().format("%m-%d %H:%M")
        )
    );
    assert_eq!(
        state.node_title().as_deref(),
        Some("pinnacle manager console")
    );
    let lines = state.node_lines(&[f.a.clone(), f.b.clone()]);
    assert_eq!(
        lines[0],
        "Reports to the operator · 3 children · 4 projects covered"
    );
    assert!(lines[1].starts_with("Grant v3 active"), "{lines:?}");
    assert_eq!(
        lines[2],
        "Fleet 20 active · 5m 600 tok/min $0.60/h · 1h 600 tok/min $0.60/h · 24h 410 invocations"
    );
    assert_eq!(
        lines[3],
        "1 escalation waiting on this node: Two globals claim the release train"
    );
}

#[test]
fn a_project_and_an_area_console_show_their_seat_and_child_areas() {
    let f = super::tests::fixture();
    let state = node_state(project_node(&f), Utc::now());
    let area = format!("area {}", &AREA.to_string()[..8]);
    assert_eq!(
        shape(&state),
        vec![
            (SeatLevel::Project, 0, "rsi".into(), SeatHealth::Active),
            (SeatLevel::Area, 1, area.clone(), SeatHealth::Idle),
        ]
    );
    assert_eq!(state.seats()[0].session_id, Some(f.pm_id));
    let lines = state.node_lines(&[f.a.clone(), f.b.clone()]);
    assert_eq!(
        lines[0],
        format!(
            "Reports to portfolio node {} · 1 child · 1 project covered",
            &GLOBAL_1.to_string()[..8]
        )
    );
    assert_eq!(lines[1], "PM scope v2 · policy v1 Execute");

    let state = node_state(area_node(&f), Utc::now());
    assert_eq!(
        shape(&state),
        vec![(SeatLevel::Area, 0, area, SeatHealth::Idle)]
    );
    let lines = state.node_lines(&[f.a.clone(), f.b.clone()]);
    assert_eq!(
        lines[0],
        "Reports to project rsi · 0 children · 1 project covered"
    );
    assert_eq!(
        lines[1],
        "Area grant v4 active · 0 groups, 1 epic in project rsi"
    );
}

#[tokio::test]
async fn enter_on_a_child_opens_its_console_and_backspace_goes_up() {
    let f = super::tests::fixture();
    let mut app = crate::app::app_test_helpers::with_session_list(0);
    app.projects = vec![f.a.clone(), f.b.clone()];
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetManagerNodeWorkspace",
        serde_json::to_value(pinnacle_node(&f)).unwrap(),
    );
    open_node(&mut app, ManagerNodeRefV1::Portfolio { node_id: PINNACLE }).await;
    assert_eq!(
        daemon.last_params("GetManagerNodeWorkspace").unwrap(),
        serde_json::json!({"node": {"kind": "portfolio", "node_id": PINNACLE}})
    );
    let OverlayState::GlobalManagerWorkspace(state) = &app.overlay else {
        panic!("console open");
    };
    assert_eq!(state.seats().len(), 7);

    // Down to global 1's row; Enter opens its console.
    daemon.serve(
        "GetManagerNodeWorkspace",
        serde_json::to_value(global_node(&f)).unwrap(),
    );
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(
        daemon.last_params("GetManagerNodeWorkspace").unwrap(),
        serde_json::json!({"node": {"kind": "portfolio", "node_id": GLOBAL_1}})
    );
    let OverlayState::GlobalManagerWorkspace(state) = &app.overlay else {
        panic!("console open");
    };
    assert_eq!(
        state.target,
        Some(ManagerNodeRefV1::Portfolio { node_id: GLOBAL_1 })
    );
    assert_eq!(state.seats()[0].label, "global manager");
    assert_eq!(daemon.calls("GetGlobalManagerWorkspace"), 0);

    // Backspace goes up to the parent node.
    daemon.serve(
        "GetManagerNodeWorkspace",
        serde_json::to_value(pinnacle_node(&f)).unwrap(),
    );
    handle_key(&mut app, key(KeyCode::Backspace)).await;
    assert_eq!(
        daemon.last_params("GetManagerNodeWorkspace").unwrap(),
        serde_json::json!({"node": {"kind": "portfolio", "node_id": PINNACLE}})
    );
    // `r` re-reads the same node.
    handle_key(&mut app, key(KeyCode::Char('r'))).await;
    assert_eq!(daemon.calls("GetManagerNodeWorkspace"), 4);
    // At the top, Backspace explains instead of moving.
    handle_key(&mut app, key(KeyCode::Backspace)).await;
    let OverlayState::GlobalManagerWorkspace(state) = &app.overlay else {
        panic!("console open");
    };
    assert!(
        state
            .error
            .as_deref()
            .is_some_and(|e| e.contains("no manager node above"))
    );
}

#[tokio::test]
async fn n_in_a_node_console_launches_only_project_managers() {
    let f = super::tests::fixture();
    let mut app = crate::app::app_test_helpers::with_session_list(0);
    app.projects = vec![f.a.clone(), f.b.clone()];
    app.overlay =
        OverlayState::GlobalManagerWorkspace(Box::new(node_state(pinnacle_node(&f), Utc::now())));
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    let OverlayState::GlobalManagerWorkspace(state) = &app.overlay else {
        panic!("console open");
    };
    assert!(state.launch.is_none());
    assert!(
        state
            .error
            .as_deref()
            .is_some_and(|e| e.contains("launches a project manager")),
        "{:?}",
        state.error
    );
    // The project row under global 1 opens the PM form.
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    let OverlayState::GlobalManagerWorkspace(state) = &app.overlay else {
        panic!("console open");
    };
    assert!(state.launch.is_some());
    // `t` selects the node's own seat.
    let mut state = node_state(pinnacle_node(&f), Utc::now());
    state.selected = 3;
    app.overlay = OverlayState::GlobalManagerWorkspace(Box::new(state));
    handle_key(&mut app, key(KeyCode::Char('t'))).await;
    let OverlayState::GlobalManagerWorkspace(state) = &app.overlay else {
        panic!("console open");
    };
    assert_eq!(state.selected, 0);
}
