//! `:manager tree` (#890 Slice C v0): a read-only, keyboard-navigable tree of
//! the manager hierarchy (global grant, project seats, area nodes, led Epics)
//! from one bounded operator snapshot (`GetManagerTree`). Appointment, scope,
//! grant and revocation stay on `:manager appoint`, `:manager global` and
//! `:manager node`; the view shows those hints and jumps to a node's session.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::manager_tree::{
    GetManagerTreeRequestV1, GetManagerTreeResultV1, ManagerTreeKindV1, ManagerTreeRowV1,
};
use uuid::Uuid;

use crate::app::App;
use crate::types::OverlayState;

/// Rows requested per RPC page.
pub const PAGE_LIMIT: u16 = 200;
/// Pages fetched on open and refresh before the operator pages on demand.
const AUTO_PAGES: usize = 5;

pub const HINTS: &str = "j/k move · h/l fold · Enter open session · n more · r refresh · Esc close   |   edit: :manager appoint · :manager global · :manager node";

#[derive(Debug, Default)]
pub struct ManagerTreeState {
    pub rows: Vec<ManagerTreeRowV1>,
    pub total_rows: u64,
    pub complete: bool,
    pub next_after: Option<String>,
    pub global_grant_version: Option<i64>,
    pub selected: usize,
    pub collapsed: HashSet<String>,
    pub error: Option<String>,
}

impl ManagerTreeState {
    /// Indices into `rows` of rows not hidden by a collapsed ancestor.
    pub fn visible(&self) -> Vec<usize> {
        let mut hidden: HashSet<&str> = HashSet::new();
        let mut out = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            if row
                .parent_key
                .as_deref()
                .is_some_and(|parent| hidden.contains(parent))
            {
                hidden.insert(row.key.as_str());
                continue;
            }
            if self.collapsed.contains(&row.key) {
                hidden.insert(row.key.as_str());
            }
            out.push(index);
        }
        // A collapsed row's own descendants are skipped above; the row stays.
        out
    }

    pub fn has_children(&self, index: usize) -> bool {
        let key = &self.rows[index].key;
        self.rows
            .iter()
            .any(|row| row.parent_key.as_deref() == Some(key.as_str()))
    }

    pub fn selected_row(&self) -> Option<&ManagerTreeRowV1> {
        self.visible()
            .get(self.selected)
            .and_then(|index| self.rows.get(*index))
    }

    /// Replace the loaded rows, keeping the selection on the same row key.
    pub fn install(&mut self, rows: Vec<ManagerTreeRowV1>, page: &GetManagerTreeResultV1) {
        let keep = self.selected_row().map(|row| row.key.clone());
        self.rows = rows;
        self.total_rows = page.total_rows;
        self.complete = page.complete;
        self.next_after = page.next_after.clone();
        self.global_grant_version = page.global_grant_version;
        let visible = self.visible();
        self.selected = keep
            .and_then(|key| {
                visible
                    .iter()
                    .position(|index| self.rows[*index].key == key)
            })
            .unwrap_or(0)
            .min(visible.len().saturating_sub(1));
    }

    pub fn count_line(&self) -> String {
        let loaded = self.rows.len() as u64;
        let mut line = if loaded < self.total_rows {
            format!("{loaded} of {} nodes loaded (n: more)", self.total_rows)
        } else {
            format!("{} nodes", self.total_rows)
        };
        if !self.complete {
            line.push_str(" · traversal incomplete: counts marked ? are unknown");
        }
        if let Some(version) = self.global_grant_version {
            line.push_str(&format!(" · global grant v{version}"));
        }
        line
    }
}

fn count(value: Option<i64>) -> String {
    value.map_or_else(|| "?".into(), |n| n.to_string())
}

fn kind_tag(kind: ManagerTreeKindV1) -> &'static str {
    match kind {
        ManagerTreeKindV1::Global => "GLOBAL",
        ManagerTreeKindV1::Project => "PROJECT",
        ManagerTreeKindV1::Area => "AREA",
        ManagerTreeKindV1::Epic => "EPIC",
    }
}

/// One row as plain text (no indentation or fold marker).
pub fn row_text(row: &ManagerTreeRowV1) -> String {
    let mut parts = vec![format!("{} {}", kind_tag(row.kind), row.label)];
    match &row.seat {
        Some(seat) => {
            let mut seat_text = format!("{:?}", seat.status);
            if let Some(model) = &seat.model {
                seat_text.push_str(&format!(" {model}"));
            }
            seat_text.push_str(&match seat.context_fill_pct {
                Some(pct) => format!(" ctx {pct:.0}%"),
                None => " ctx ?".into(),
            });
            seat_text.push_str(&format!(" {}", seat.updated_at.format("%m-%d %H:%M")));
            parts.push(seat_text);
        }
        None if row.focus_session_id.is_some() => parts.push("seat session missing".into()),
        None => parts.push("no seat".into()),
    }
    if let Some(scope) = &row.scope {
        parts.push(scope.clone());
    }
    if let Some(grant) = &row.grant {
        let caps: Vec<String> = grant
            .capabilities
            .iter()
            .map(|c| format!("{c:?}"))
            .collect();
        parts.push(format!("caps {}", caps.join("+")));
        let mut allowance = format!(
            "allow active {} sessions {} reports {}",
            grant.max_active_sessions, grant.max_created_sessions, grant.max_direct_reports
        );
        for reserved in &grant.reserved {
            allowance.push_str(&format!(
                " reserved {}={}",
                reserved.resource_kind, reserved.amount
            ));
        }
        parts.push(allowance);
    }
    parts.push(format!(
        "run {} reports {} esc {} dec {}",
        count(row.load.running_workers),
        count(row.load.direct_reports),
        count(row.load.pending_escalations),
        count(row.load.pending_decisions),
    ));
    if !row.complete {
        parts.push("children incomplete".into());
    }
    parts.join(" · ")
}

fn state(app: &mut App) -> Option<&mut ManagerTreeState> {
    match &mut app.overlay {
        OverlayState::ManagerTree(state) => Some(state),
        _ => None,
    }
}

/// Fetch pages (from the start, or continuing after `after`) into `rows`.
async fn fetch(
    app: &mut App,
    mut after: Option<String>,
    mut rows: Vec<ManagerTreeRowV1>,
    pages: usize,
) -> Result<(Vec<ManagerTreeRowV1>, GetManagerTreeResultV1), String> {
    let mut last = None;
    for _ in 0..pages {
        let page = app
            .client
            .get_manager_tree(GetManagerTreeRequestV1 {
                after: after.clone(),
                limit: PAGE_LIMIT,
            })
            .await
            .map_err(|error| error.to_string())?;
        rows.extend(page.rows.iter().cloned());
        after = page.next_after.clone();
        let done = after.is_none();
        last = Some(page);
        if done {
            break;
        }
    }
    let mut page = last.ok_or_else(|| "empty manager tree request".to_string())?;
    page.rows = Vec::new();
    Ok((rows, page))
}

pub async fn open(app: &mut App) {
    match fetch(app, None, Vec::new(), AUTO_PAGES).await {
        Ok((rows, page)) => {
            let mut tree = ManagerTreeState::default();
            tree.install(rows, &page);
            app.overlay = OverlayState::ManagerTree(Box::new(tree));
        }
        Err(error) => app.notify_error(format!("Manager tree: {error}")),
    }
    app.mark_dirty();
}

/// Reload from the start (a CAS-style refresh: the daemon snapshot is the
/// truth after a succession or rescope), keeping the selected row.
async fn refresh(app: &mut App) {
    match fetch(app, None, Vec::new(), AUTO_PAGES).await {
        Ok((rows, page)) => {
            if let Some(tree) = state(app) {
                tree.install(rows, &page);
                tree.error = None;
            }
        }
        Err(error) => {
            if let Some(tree) = state(app) {
                tree.error = Some(format!("Refresh failed: {error}"));
            }
        }
    }
}

async fn more(app: &mut App) {
    let Some(tree) = state(app) else { return };
    let Some(after) = tree.next_after.clone() else {
        return;
    };
    let rows = tree.rows.clone();
    match fetch(app, Some(after), rows, 1).await {
        Ok((rows, page)) => {
            if let Some(tree) = state(app) {
                tree.install(rows, &page);
                tree.error = None;
            }
        }
        // A stale cursor means the tree changed under us: reload from the start.
        Err(error) if error.contains("manager_tree_stale_cursor") => refresh(app).await,
        Err(error) => {
            if let Some(tree) = state(app) {
                tree.error = Some(format!("Load more failed: {error}"));
            }
        }
    }
}

async fn jump(app: &mut App, session_id: Uuid) {
    if !app.sessions.contains_key(&session_id) {
        match app.client.get_session(session_id).await {
            Ok(session) => {
                app.upsert_session(session.clone());
                if !app.sessions.contains_key(&session_id) {
                    app.sessions
                        .insert(session_id, crate::types::SessionState::new(session));
                }
            }
            Err(error) => {
                app.notify_error(format!("Session unavailable: {error}"));
                return;
            }
        }
    }
    app.overlay = OverlayState::None;
    app.open_session_in_current_pane(session_id);
}

pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(tree) = state(app) else { return };
    let visible = tree.visible();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Char('r') => refresh(app).await,
        KeyCode::Char('n') => more(app).await,
        KeyCode::Char('h') | KeyCode::Left => {
            if let Some(row) = tree.selected_row().cloned() {
                let target = if tree.collapsed.contains(&row.key)
                    || !tree.has_children(visible[tree.selected])
                {
                    // Fold the parent and select it.
                    row.parent_key.clone()
                } else {
                    Some(row.key.clone())
                };
                if let Some(key) = target {
                    tree.collapsed.insert(key.clone());
                    let visible = tree.visible();
                    if let Some(pos) = visible.iter().position(|i| tree.rows[*i].key == key) {
                        tree.selected = pos;
                    }
                }
            }
        }
        KeyCode::Char('l') | KeyCode::Right => {
            if let Some(row) = tree.selected_row() {
                let key = row.key.clone();
                tree.collapsed.remove(&key);
            }
        }
        KeyCode::Enter => {
            let target = tree.selected_row().and_then(|row| row.focus_session_id);
            match target {
                Some(id) => jump(app, id).await,
                None => app.notify_error("This node has no session to open."),
            }
        }
        _ => {
            let mut selected = tree.selected;
            if crate::overlay::list::handle_list_nav_key(&mut selected, visible.len(), &key) {
                tree.selected = selected;
                // Reaching the last loaded row of a partial tree pages on.
                if selected + 1 == visible.len() && tree.next_after.is_some() {
                    more(app).await;
                }
            }
        }
    }
    app.mark_dirty();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Pane;
    use crossterm::event::KeyModifiers;
    use rsi_common::manager_tree::{ManagerTreeGrantV1, ManagerTreeLoadV1, ManagerTreeSeatV1};
    use rsi_common::types::SessionStatus;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(
        key: &str,
        parent: Option<&str>,
        depth: u16,
        kind: ManagerTreeKindV1,
        focus: Option<Uuid>,
    ) -> ManagerTreeRowV1 {
        ManagerTreeRowV1 {
            key: key.into(),
            parent_key: parent.map(str::to_string),
            depth,
            kind,
            label: key.into(),
            project_id: None,
            node_id: None,
            epic_id: None,
            scope: None,
            seat: focus.map(|session_id| ManagerTreeSeatV1 {
                session_id,
                status: SessionStatus::Running,
                model: Some("claude-opus-5-5".into()),
                context_fill_pct: Some(41.6),
                updated_at: chrono::Utc::now(),
            }),
            focus_session_id: focus,
            grant: None,
            load: ManagerTreeLoadV1 {
                running_workers: Some(2),
                direct_reports: None,
                pending_escalations: Some(1),
                pending_decisions: Some(0),
            },
            complete: true,
        }
    }

    fn tree(focus: Uuid) -> ManagerTreeState {
        let mut state = ManagerTreeState::default();
        let rows = vec![
            row("global", None, 0, ManagerTreeKindV1::Global, None),
            row(
                "project:a",
                Some("global"),
                1,
                ManagerTreeKindV1::Project,
                Some(focus),
            ),
            row(
                "area:x",
                Some("project:a"),
                2,
                ManagerTreeKindV1::Area,
                None,
            ),
            row("epic:e", Some("area:x"), 3, ManagerTreeKindV1::Epic, None),
            row(
                "project:b",
                Some("global"),
                1,
                ManagerTreeKindV1::Project,
                None,
            ),
        ];
        let page = GetManagerTreeResultV1 {
            rows: vec![],
            next_after: None,
            total_rows: 5,
            complete: true,
            global_grant_version: Some(3),
        };
        state.install(rows, &page);
        state
    }

    #[tokio::test]
    async fn keys_navigate_fold_and_close() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        app.overlay = OverlayState::ManagerTree(Box::new(tree(Uuid::new_v4())));
        for code in [KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Char('j')] {
            handle_key(&mut app, key(code)).await;
        }
        let OverlayState::ManagerTree(state) = &app.overlay else {
            panic!("overlay closed")
        };
        assert_eq!(state.selected_row().unwrap().key, "epic:e");
        // h on a leaf folds its parent and selects it; the subtree disappears.
        handle_key(&mut app, key(KeyCode::Char('h'))).await;
        let OverlayState::ManagerTree(state) = &app.overlay else {
            panic!("overlay closed")
        };
        assert_eq!(state.selected_row().unwrap().key, "area:x");
        let keys: Vec<_> = state
            .visible()
            .iter()
            .map(|i| state.rows[*i].key.as_str())
            .collect();
        assert_eq!(keys, ["global", "project:a", "area:x", "project:b"]);
        handle_key(&mut app, key(KeyCode::Char('l'))).await;
        let OverlayState::ManagerTree(state) = &app.overlay else {
            panic!("overlay closed")
        };
        assert_eq!(state.visible().len(), 5);
        handle_key(&mut app, key(KeyCode::Char('G'))).await;
        handle_key(&mut app, key(KeyCode::Char('g'))).await;
        let OverlayState::ManagerTree(state) = &app.overlay else {
            panic!("overlay closed")
        };
        assert_eq!(state.selected_row().unwrap().key, "global");
        handle_key(&mut app, key(KeyCode::Esc)).await;
        assert!(matches!(app.overlay, OverlayState::None));
    }

    #[tokio::test]
    async fn enter_opens_the_focused_nodes_session() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session = *app.sessions.keys().next().unwrap();
        app.overlay = OverlayState::ManagerTree(Box::new(tree(session)));
        handle_key(&mut app, key(KeyCode::Char('j'))).await;
        handle_key(&mut app, key(KeyCode::Enter)).await;
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(
            matches!(app.focused_pane(), Some(Pane::SessionDetail { session_id }) if *session_id == session)
        );
    }

    #[tokio::test]
    async fn enter_on_a_node_without_a_session_keeps_the_view_open() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        app.overlay = OverlayState::ManagerTree(Box::new(tree(Uuid::new_v4())));
        handle_key(&mut app, key(KeyCode::Enter)).await;
        assert!(matches!(app.overlay, OverlayState::ManagerTree(..)));
    }

    #[test]
    fn counts_show_totals_and_mark_unknown_values() {
        let mut state = tree(Uuid::new_v4());
        assert_eq!(state.count_line(), "5 nodes · global grant v3");
        state.total_rows = 12;
        state.complete = false;
        let line = state.count_line();
        assert!(line.starts_with("5 of 12 nodes loaded"), "{line}");
        assert!(line.contains("traversal incomplete"), "{line}");
        let mut unknown = row("area:x", None, 0, ManagerTreeKindV1::Area, None);
        unknown.load.running_workers = None;
        unknown.complete = false;
        let text = row_text(&unknown);
        assert!(text.contains("run ? reports ? esc 1 dec 0"), "{text}");
        assert!(text.contains("children incomplete"), "{text}");
        assert!(text.contains("no seat"), "{text}");
    }

    #[test]
    fn row_text_shows_seat_health_scope_grant_and_load() {
        let mut node = row(
            "area:x",
            None,
            2,
            ManagerTreeKindV1::Area,
            Some(Uuid::new_v4()),
        );
        node.scope = Some("0 groups, 1 epic".into());
        node.grant = Some(ManagerTreeGrantV1 {
            grant_version: 2,
            capabilities: vec![rsi_common::harness_manager_v2::ManagerCapabilityV2::WorkPlan],
            max_active_sessions: 3,
            max_created_sessions: 5,
            max_created_containers: 0,
            max_direct_reports: 4,
            max_spend_usd: None,
            reserved: vec![rsi_common::manager_tree::ManagerTreeReservedV1 {
                resource_kind: "active_sessions".into(),
                amount: 2,
            }],
        });
        let text = row_text(&node);
        for needle in [
            "AREA area:x",
            "Running claude-opus-5-5 ctx 42%",
            "0 groups, 1 epic",
            "caps WorkPlan",
            "allow active 3 sessions 5 reports 4 reserved active_sessions=2",
            "run 2 reports ? esc 1 dec 0",
        ] {
            assert!(text.contains(needle), "{needle} missing from {text}");
        }
    }

    #[test]
    fn install_keeps_the_selection_on_the_same_row_after_a_refresh() {
        let mut state = tree(Uuid::new_v4());
        state.selected = 4;
        assert_eq!(state.selected_row().unwrap().key, "project:b");
        let mut rows = state.rows.clone();
        rows.remove(3); // the Epic moved away between refreshes
        let page = GetManagerTreeResultV1 {
            rows: vec![],
            next_after: None,
            total_rows: 4,
            complete: true,
            global_grant_version: Some(4),
        };
        state.install(rows, &page);
        assert_eq!(state.selected_row().unwrap().key, "project:b");
        assert_eq!(state.total_rows, 4);
        assert_eq!(state.global_grant_version, Some(4));
    }
}
