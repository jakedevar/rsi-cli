use super::*;
use crate::overlay::manager_tree::actions::PreparedRequest;
use crate::overlay::manager_tree::{Candidate, Notice};
use chrono::TimeZone;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use rsi_common::manager_nodes::{
    ManagerNodeAllowanceV1, ManagerNodeGrantStateV1, ManagerNodeGrantV1, ManagerNodeSelectorV1,
    ManagerNodeStateV1, ManagerNodeViewV1, RevokeManagerNodeRequestV1,
};
use rsi_common::manager_tree::{
    GetManagerTreeResultV1, ManagerTreeGrantV1, ManagerTreeLoadV1, ManagerTreeRowV1,
    ManagerTreeSeatV1,
};
use rsi_common::types::SessionStatus;
use uuid::Uuid;

fn at() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc
        .with_ymd_and_hms(2026, 10, 5, 19, 40, 0)
        .unwrap()
}

fn uuid(n: u128) -> Uuid {
    Uuid::from_u128(0x1214_0000_0000_4000_8000_0000_0000_0000 | n)
}

#[allow(clippy::too_many_arguments)]
fn node(
    key: &str,
    parent: Option<&str>,
    depth: u16,
    kind: ManagerTreeKindV1,
    label: &str,
    seat: Option<(u128, SessionStatus, f64)>,
    scope: Option<&str>,
    load: [Option<i64>; 4],
) -> ManagerTreeRowV1 {
    ManagerTreeRowV1 {
        key: key.into(),
        parent_key: parent.map(str::to_string),
        depth,
        kind,
        label: label.into(),
        tier_label: None,
        grantor: None,
        project_id: Some(uuid(99)),
        node_id: None,
        epic_id: None,
        scope: scope.map(str::to_string),
        seat: seat.map(|(id, status, ctx)| ManagerTreeSeatV1 {
            session_id: uuid(id),
            status,
            model: Some("claude-opus-5-5".into()),
            context_fill_pct: Some(ctx),
            updated_at: at(),
        }),
        focus_session_id: seat.map(|(id, ..)| uuid(id)),
        grant: None,
        launches: Vec::new(),
        load: ManagerTreeLoadV1 {
            running_workers: load[0],
            direct_reports: load[1],
            pending_escalations: load[2],
            pending_decisions: load[3],
        },
        complete: true,
    }
}

fn grant(version: i64, reports: u16) -> ManagerTreeGrantV1 {
    ManagerTreeGrantV1 {
        grant_version: version,
        capabilities: vec![
            rsi_common::harness_manager_v2::ManagerCapabilityV2::WorkPlan,
            rsi_common::harness_manager_v2::ManagerCapabilityV2::SessionCreate,
        ],
        max_active_sessions: 3,
        max_created_sessions: 6,
        max_created_containers: 0,
        max_direct_reports: reports,
        max_spend_usd: None,
        reserved: vec![rsi_common::manager_tree::ManagerTreeReservedV1 {
            resource_kind: "active_sessions".into(),
            amount: 1,
        }],
    }
}

/// A realistic hierarchy: global grant, two granted projects (one with area
/// nodes and Epics), one ungranted project, a revoked node, a missing seat and
/// an incomplete traversal.
fn sample_rows() -> Vec<ManagerTreeRowV1> {
    use ManagerTreeKindV1::{Area, Epic, Global, Project};
    use SessionStatus::{Completed, Failed, Running, WaitingApproval};
    let mut rows = vec![
        node(
            "global",
            None,
            0,
            Global,
            "global manager",
            Some((1, Running, 23.0)),
            Some("2 granted project(s)"),
            [None, Some(2), Some(3), Some(1)],
        ),
        node(
            "project:rsi",
            Some("global"),
            1,
            Project,
            "rsi",
            Some((2, Running, 61.0)),
            Some("project"),
            [Some(7), Some(2), Some(2), Some(1)],
        ),
        node(
            "area:tui",
            Some("project:rsi"),
            2,
            Area,
            "area 5f2a91c0",
            Some((3, WaitingApproval, 38.0)),
            Some("1 group(s), 0 epic(s)"),
            [Some(3), Some(1), Some(2), Some(0)],
        ),
        node(
            "epic:tree",
            Some("area:tui"),
            3,
            Epic,
            "Manager tree redesign",
            Some((4, Running, 12.0)),
            None,
            [Some(2), None, None, Some(1)],
        ),
        node(
            "area:daemon",
            Some("project:rsi"),
            2,
            Area,
            "area 0be3d7aa (revoked)",
            None,
            Some("revoked"),
            [Some(0), Some(0), Some(0), Some(0)],
        ),
        node(
            "epic:rpc",
            Some("project:rsi"),
            2,
            Epic,
            "Operator RPC hardening",
            Some((5, Completed, 88.0)),
            None,
            [Some(0), None, None, Some(0)],
        ),
        node(
            "project:notes",
            Some("global"),
            1,
            Project,
            "notes",
            None,
            Some("no manager appointed"),
            [Some(0), None, Some(0), Some(0)],
        ),
        node(
            "project:dictate",
            None,
            0,
            Project,
            "dictate agent",
            Some((6, Failed, 97.0)),
            Some("2 group(s), 3 epic(s)"),
            [None, Some(0), None, None],
        ),
    ];
    rows[2].grant = Some(grant(4, 2));
    rows[2].node_id = Some(uuid(40));
    rows[6].focus_session_id = None;
    // A seat id the daemon could not load.
    rows[7].focus_session_id = Some(uuid(66));
    rows[7].seat = None;
    rows[7].complete = false;
    rows
}

fn sample_state() -> ManagerTreeState {
    let mut state = ManagerTreeState::default();
    let rows = sample_rows();
    let total = rows.len() as u64;
    state.install(
        rows,
        &GetManagerTreeResultV1 {
            rows: vec![],
            next_after: None,
            total_rows: total,
            complete: false,
            global_grant_version: Some(3),
        },
    );
    state.loaded_at = Some(at());
    state.selected = 2;
    state.candidate = Some(Candidate {
        id: uuid(77),
        name: "Fixture session 00".into(),
        project_id: Some(uuid(99)),
        eligible: true,
    });
    state
}

/// 60 projects under the global seat with only the first page loaded.
fn large_state() -> ManagerTreeState {
    let mut rows = vec![node(
        "global",
        None,
        0,
        ManagerTreeKindV1::Global,
        "global manager",
        Some((1, SessionStatus::Running, 40.0)),
        Some("140 granted project(s)"),
        [None, Some(140), Some(0), Some(0)],
    )];
    for n in 0..59 {
        rows.push(node(
            &format!("project:{n:03}"),
            Some("global"),
            1,
            ManagerTreeKindV1::Project,
            &format!("project {n:03}"),
            Some((100 + n, SessionStatus::Running, 10.0 + n as f64)),
            Some("project"),
            [
                Some(n as i64 % 4),
                Some(1),
                Some(i64::from(n % 7 == 0)),
                Some(0),
            ],
        ));
    }
    let mut state = ManagerTreeState::default();
    state.install(
        rows,
        &GetManagerTreeResultV1 {
            rows: vec![],
            next_after: Some("project:058".into()),
            total_rows: 141,
            complete: true,
            global_grant_version: Some(12),
        },
    );
    state.loaded_at = Some(at());
    state.selected = 45;
    state
}

fn draw(state: &ManagerTreeState, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| render(frame, frame.area(), state))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
    let area = buffer.area;
    for y in 0..area.height {
        let mut line = String::new();
        let mut xs = Vec::new();
        for x in 0..area.width {
            let symbol = buffer[(x, y)].symbol();
            for _ in symbol.chars() {
                xs.push(x);
            }
            line.push_str(symbol);
        }
        if let Some(byte) = line.find(needle) {
            let index = line[..byte].chars().count();
            return Some((xs[index], y));
        }
    }
    None
}

#[test]
fn lines_draw_guides_fold_markers_and_hide_collapsed_children() {
    let mut state = sample_state();
    let lines = tree_lines(&state);
    assert_eq!(lines.len(), 8);
    assert!(lines[0].starts_with("▾ GLOBAL global manager"), "{lines:?}");
    assert!(lines[1].starts_with("├─▾ PROJECT rsi"), "{lines:?}");
    assert!(
        lines[2].starts_with("│ ├─▾ AREA area 5f2a91c0"),
        "{lines:?}"
    );
    assert!(
        lines[3].starts_with("│ │ └─· EPIC Manager tree redesign"),
        "{lines:?}"
    );
    assert!(lines[6].starts_with("└─· PROJECT notes"), "{lines:?}");
    assert!(lines[7].starts_with("· PROJECT dictate agent"), "{lines:?}");
    state.collapsed.insert("project:rsi".into());
    let lines = tree_lines(&state);
    assert_eq!(lines.len(), 4);
    assert!(lines[1].starts_with("├─▸ PROJECT rsi"), "{lines:?}");
}

#[test]
fn the_main_view_separates_hierarchy_health_scope_and_load() {
    let state = sample_state();
    let screen = text(&draw(&state, 200, 50));
    for needle in [
        "Manager tree",
        "8 nodes · traversal incomplete: counts marked ? are unknown · global grant v3 · snapshot",
        "rows 1–8 of 8",
        "NODE",
        "SEAT HEALTH",
        "SCOPE · GRANT",
        "LOAD",
        "├─▾ PROJECT rsi",
        "● Waiting ctx 38% 19:40 claude-opus-5-5",
        "○ seat session missing",
        "○ no seat",
        "1 group(s), 0 epic(s) · v4 caps WorkPlan+SessionCreate",
        "esc 2",
        "! partial",
        "── AREA area 5f2a91c0",
        "a replace seat",
        "e edit allowance",
        "x revoke subtree",
        "a replace seat: no RPC moves an area seat",
    ] {
        assert!(screen.contains(needle), "{needle} missing from\n{screen}");
    }
}

#[test]
fn every_size_keeps_the_selection_visible_and_marks_unloaded_rows() {
    let state = large_state();
    for (width, height) in [(80, 24), (120, 40), (200, 50), (60, 12)] {
        let buffer = draw(&state, width, height);
        let screen = text(&buffer);
        assert!(
            screen.contains("project 044"),
            "selected row hidden at {width}x{height}\n{screen}"
        );
        assert!(
            screen.contains("81 more node(s) not loaded"),
            "unloaded marker missing at {width}x{height}\n{screen}"
        );
        assert!(
            screen.contains("60 of 141 nodes loaded"),
            "count missing at {width}x{height}\n{screen}"
        );
        assert!(state.viewport.get() >= 1);
        let first = state.scroll.get();
        assert!(first <= 45 && 45 < first + state.viewport.get());
    }
    // A short terminal gives the detail pane's rows to the tree.
    let tall = text(&draw(&state, 120, 26));
    assert!(tall.contains("── PROJECT project 044"), "{tall}");
    let short = text(&draw(&state, 120, 16));
    assert!(short.contains("├─· PROJECT project 044"), "{short}");
    assert!(short.contains("rows "), "{short}");
}

#[test]
fn narrow_widths_drop_the_scope_column_but_keep_load() {
    let state = sample_state();
    let screen = text(&draw(&state, 80, 30));
    assert!(screen.contains("LOAD"), "{screen}");
    assert!(screen.contains("run 7"), "{screen}");
    assert!(screen.contains("SEAT HEALTH"), "{screen}");
    // The detail pane still carries the full scope.
    assert!(screen.contains("1 group(s), 0 epic(s)"), "{screen}");
}

#[test]
fn theme_colours_mark_selection_kinds_and_alerts() {
    let _theme = crate::ui::theme::pin_theme_state();
    let state = sample_state();
    let buffer = draw(&state, 200, 50);
    let (x, y) = find(&buffer, "AREA    area 5f2a91c0").unwrap();
    assert_eq!(buffer[(x, y)].bg, theme::selected_row_bg());
    assert_eq!(buffer[(x, y)].fg, theme::model_text());
    let (x, y) = find(&buffer, "GLOBAL").unwrap();
    assert_eq!(buffer[(x, y)].fg, theme::accent());
    // A pending escalation stands out from a zero count.
    let (gx, gy) = find(&buffer, "esc 3").unwrap();
    assert_eq!(buffer[(gx + 4, gy)].fg, theme::warning_status());
    let (px, py) = find(&buffer, "Waiting ctx").unwrap();
    assert_eq!(
        buffer[(px, py)].fg,
        theme::status_color(SessionStatus::WaitingApproval)
    );
}

fn pending_revoke(state: &ManagerTreeState) -> PendingAction {
    let mut lines = vec![
        "Revokes area node 5f2a91c0 (seat 12140000) and the authority it delegated below it."
            .into(),
    ];
    lines.extend(state.impact_lines(2));
    PendingAction {
        title: "Revoke area 5f2a91c0".into(),
        destructive: true,
        lines,
        rpc: "RevokeManagerNode (expects grant v4 · epoch 7)".into(),
        request: PreparedRequest::RevokeNode(RevokeManagerNodeRequestV1 {
            project_id: uuid(99),
            node_id: uuid(40),
            expected_grant_version: 4,
            expected_authority_epoch: 7,
            idempotency_key: "k".into(),
        }),
    }
}

#[test]
fn a_destructive_confirm_shows_impact_rpc_and_keys() {
    let mut state = sample_state();
    state.modal = Some(TreeModal::Confirm(Box::new(pending_revoke(&state))));
    let screen = text(&draw(&state, 120, 40));
    for needle in [
        "! Revoke area 5f2a91c0",
        "1 descendant node(s) loaded: 0 project(s), 0 area node(s), 1 Epic(s)",
        "Affected seats (2):",
        "RPC: RevokeManagerNode (expects grant v4 · epoch 7)",
        "y confirm · n/Esc cancel (destructive: Enter does not confirm)",
    ] {
        assert!(screen.contains(needle), "{needle} missing from\n{screen}");
    }
    // Too small for every line: the modal says how many it left out.
    let tiny = text(&draw(&state, 120, 12));
    assert!(
        tiny.contains("more line(s): enlarge the terminal"),
        "{tiny}"
    );
}

fn node_view(id: u128, version: i64) -> ManagerNodeViewV1 {
    let policy = crate::overlay::global_manager_command::default_project_policy();
    ManagerNodeViewV1 {
        node_id: uuid(id),
        parent_node_id: Some(uuid(1)),
        seat_root_session_id: uuid(3),
        project_id: uuid(99),
        selector: Some(ManagerNodeSelectorV1::Project),
        state: ManagerNodeStateV1::Active,
        grant_state: ManagerNodeGrantStateV1::Granted,
        grant: Some(ManagerNodeGrantV1 {
            capabilities: policy.capabilities.clone(),
            allowed_launches: policy.allowed_launches.clone(),
            allowance: ManagerNodeAllowanceV1 {
                max_created_containers: 0,
                max_created_sessions: 6,
                max_active_sessions: 3,
                max_build_slots: 0,
                max_disk_gib: 0,
                provider_limits: vec![],
                max_spend_usd: None,
            },
            max_direct_reports: 2,
        }),
        policy: Some(policy),
        grant_version: version,
        policy_version: 1,
        authority_epoch: 7,
        direct_reports: 0,
        updated_at: at(),
    }
}

fn editors() -> (NodeEditor, GlobalEditor) {
    let node = NodeEditor {
        row_label: "area 5f2a91c0".into(),
        node: node_view(40, 4),
        parent: node_view(1, 9),
        values: [2, 6, 2],
        original: [3, 6, 2],
        cursor: 0,
        error: Some("The daemon refused: manager_node_grant_not_narrower".into()),
    };
    let now = at();
    let mut global = GlobalEditor::new(
        rsi_common::global_manager::GlobalManagerGrantV1 {
            grant_id: uuid(5),
            grant_version: 3,
            seat_session_id: uuid(1),
            state: "active".into(),
            project_ids: vec![uuid(10), uuid(11)],
            allowed_launches: vec![],
            project_policy: crate::overlay::global_manager_command::default_project_policy(),
            operator_origin: "operator_rpc".into(),
            created_at: now,
            updated_at: now,
        },
        None,
        vec![
            (uuid(12), "dictate agent".into(), false),
            (uuid(11), "notes".into(), true),
            (uuid(10), "rsi".into(), true),
        ],
    );
    global.cursor = 1;
    global.caps.max_active_sessions = 12;
    (node, global)
}

#[test]
fn editors_show_current_values_bounds_and_keys() {
    let (node, global) = editors();
    let mut state = sample_state();
    state.modal = Some(TreeModal::EditNode(Box::new(node)));
    let screen = text(&draw(&state, 120, 40));
    for needle in [
        "Edit allowance · area 5f2a91c0",
        "max active sessions    ◂    2 ▸   was 3    parent 3",
        "manager_node_grant_not_narrower",
        "j/k field · h/l or -/+ adjust · Enter review · Esc cancel",
    ] {
        assert!(screen.contains(needle), "{needle} missing from\n{screen}");
    }
    state.modal = Some(TreeModal::EditGlobal(Box::new(global)));
    let screen = text(&draw(&state, 120, 40));
    for needle in [
        "[ ] dictate agent",
        "[x] notes",
        "[x] rsi",
        "Space toggle",
        "max active sessions",
        "◂     12 ▸   was 4",
        "h/l adjust a cap",
    ] {
        assert!(screen.contains(needle), "{needle} missing from\n{screen}");
    }
}

/// Text dumps of the main states for the Issue's visual check. Set
/// `RSI_MANAGER_TREE_DUMP_DIR` to write them.
#[test]
fn visual_dumps() {
    let mut cases: Vec<(String, ManagerTreeState, u16, u16)> = Vec::new();
    for (width, height) in [(120, 40), (200, 50)] {
        cases.push((
            format!("main-{width}x{height}"),
            sample_state(),
            width,
            height,
        ));
        cases.push((
            format!("paged-{width}x{height}"),
            large_state(),
            width,
            height,
        ));
        let mut preview = sample_state();
        preview.selected = 1;
        let lines = preview.impact_lines(1);
        preview.modal = Some(TreeModal::Preview {
            title: "Impact of PROJECT rsi".into(),
            lines,
        });
        cases.push((format!("preview-{width}x{height}"), preview, width, height));
        let mut confirm = sample_state();
        confirm.modal = Some(TreeModal::Confirm(Box::new(pending_revoke(&confirm))));
        cases.push((
            format!("confirm-revoke-{width}x{height}"),
            confirm,
            width,
            height,
        ));
        let (node, global) = editors();
        let mut edit = sample_state();
        edit.modal = Some(TreeModal::EditNode(Box::new(node)));
        cases.push((format!("edit-node-{width}x{height}"), edit, width, height));
        let mut edit = sample_state();
        edit.selected = 0;
        edit.modal = Some(TreeModal::EditGlobal(Box::new(global)));
        cases.push((format!("edit-global-{width}x{height}"), edit, width, height));
        let mut outcome = sample_state();
        outcome.notice = Some(Notice {
            text: "Refused: the node changed after the preview (RPC error (-32000): manager_node_stale_update). Tree refreshed. Review and retry.".into(),
            tone: Tone::Error,
        });
        cases.push((
            format!("outcome-refused-{width}x{height}"),
            outcome,
            width,
            height,
        ));
    }
    cases.push(("narrow-80x24".into(), sample_state(), 80, 24));
    let directory = std::env::var_os("RSI_MANAGER_TREE_DUMP_DIR");
    for (name, state, width, height) in cases {
        let screen = text(&draw(&state, width, height));
        assert!(screen.contains("Manager tree"), "{name}");
        if let Some(directory) = &directory {
            let path = std::path::Path::new(directory).join(format!("{name}.txt"));
            std::fs::write(path, screen).unwrap();
        }
    }
}

/// #1239: every seat row names its grantor: the operator, or the granting
/// node by its tier label and short id (in the scope column and the detail).
#[test]
fn rows_show_who_granted_each_seat() {
    use ManagerTreeKindV1::{Portfolio, Project};
    use SessionStatus::Running;
    let mut pinnacle = node(
        "portfolio:pinnacle",
        None,
        0,
        Portfolio,
        "pinnacle manager",
        Some((1, Running, 10.0)),
        Some("2 granted project(s)"),
        [None, Some(1), None, None],
    );
    pinnacle.tier_label = Some("pinnacle".into());
    pinnacle.node_id = Some(uuid(10));
    pinnacle.grantor = Some("operator".into());
    let mut area = node(
        "portfolio:area",
        Some("portfolio:pinnacle"),
        1,
        Portfolio,
        "area-lead manager",
        Some((2, Running, 11.0)),
        Some("1 granted project(s)"),
        [None, Some(1), None, None],
    );
    area.tier_label = Some("area-lead".into());
    area.node_id = Some(uuid(11));
    area.grantor = Some(format!("node:{}", uuid(10)));
    let mut pm = node(
        "project:b",
        Some("portfolio:area"),
        2,
        Project,
        "project B",
        Some((3, Running, 12.0)),
        Some("project"),
        [Some(1), None, None, None],
    );
    pm.grantor = Some(format!("node:{}", uuid(11)));
    let rows = vec![pinnacle, area, pm];
    let mut state = ManagerTreeState::default();
    state.install(
        rows,
        &GetManagerTreeResultV1 {
            rows: vec![],
            next_after: None,
            total_rows: 3,
            complete: true,
            global_grant_version: None,
        },
    );
    state.loaded_at = Some(at());
    state.selected = 1;
    let pinnacle_short = &uuid(10).to_string()[..8];
    let area_short = &uuid(11).to_string()[..8];
    let screen = text(&draw(&state, 220, 40));
    for needle in [
        "2 granted project(s) · by operator".to_string(),
        format!("1 granted project(s) · by pinnacle {pinnacle_short}"),
        format!("project · by area-lead {area_short}"),
        format!("granted by pinnacle {pinnacle_short}"),
    ] {
        assert!(screen.contains(&needle), "{needle} missing from\n{screen}");
    }
}
