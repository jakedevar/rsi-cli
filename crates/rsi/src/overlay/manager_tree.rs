//! `:manager tree`: a keyboard-navigable tree of the manager hierarchy (global
//! grant, project seats, area nodes, led Epics) from one bounded operator
//! snapshot (`GetManagerTree`, #890), with in-tree operator actions (#1214).
//!
//! Every action goes through an existing typed operator RPC (see
//! [`actions`]); destructive ones open a confirm step that shows the
//! descendant impact first. Every RPC carries the version the operator
//! reviewed, and the tree reloads after each outcome so it never shows stale
//! state as current.

pub(crate) mod actions;
#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::collections::HashSet;

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::manager_tier_routing::ManagerNodeRefV1;
use rsi_common::manager_tree::{
    GetManagerTreeRequestV1, GetManagerTreeResultV1, ManagerTreeKindV1, ManagerTreeRowV1,
};
use rsi_common::types::SessionProvider;
use uuid::Uuid;

use crate::app::App;
use crate::types::OverlayState;

pub use actions::{
    ActionAvailability, Candidate, GlobalEditor, NodeEditor, PendingAction, PreparedRequest,
    TreeAction, TreeModal,
};
pub(crate) use actions::{offer_cap_confirmation, offer_grant_addition};

/// Rows requested per RPC page.
pub const PAGE_LIMIT: u16 = 200;
/// Pages fetched on open and refresh before the operator pages on demand.
const AUTO_PAGES: usize = 5;
/// Body rows a page key moves when the view has not been drawn yet.
const DEFAULT_VIEWPORT: usize = 10;

pub const HINTS: &str = "j/k move · h/l fold · Space toggle · H/L fold all · PgUp/PgDn page · n more · r refresh · Enter seat · o project · p preview · a appoint · e edit · x revoke · A manager above · m move under · Esc close";

/// Selection and folds kept on [`App`] while the tree is closed, so jumping
/// to a seat and reopening `:manager tree` lands on the same node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeMemory {
    pub selected_key: Option<String>,
    pub collapsed: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Error,
}

/// The outcome of the last action or refresh, shown above the hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub tone: Tone,
}

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
    pub notice: Option<Notice>,
    pub modal: Option<TreeModal>,
    /// The session focused behind the overlay: the seat an appoint uses.
    pub candidate: Option<Candidate>,
    /// #1237: the portfolio node a pending "move under" moves (id, label).
    pub move_source: Option<(Uuid, String)>,
    pub loaded_at: Option<DateTime<Utc>>,
    /// Body rows drawn last frame (page size); written by the renderer.
    pub viewport: Cell<usize>,
    /// First drawn body row; the renderer keeps the selection inside it.
    pub scroll: Cell<usize>,
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

    pub fn selected_index(&self) -> Option<usize> {
        self.visible().get(self.selected).copied()
    }

    pub fn selected_row(&self) -> Option<&ManagerTreeRowV1> {
        self.selected_index().and_then(|index| self.rows.get(index))
    }

    fn select_key(&mut self, key: &str) -> bool {
        match self
            .visible()
            .iter()
            .position(|index| self.rows[*index].key == key)
        {
            Some(position) => {
                self.selected = position;
                true
            }
            None => false,
        }
    }

    /// Replace the loaded rows, keeping the selection on the same row key.
    pub fn install(&mut self, rows: Vec<ManagerTreeRowV1>, page: &GetManagerTreeResultV1) {
        let keep = self.selected_row().map(|row| row.key.clone());
        self.rows = rows;
        self.total_rows = page.total_rows;
        self.complete = page.complete;
        self.next_after = page.next_after.clone();
        self.global_grant_version = page.global_grant_version;
        self.loaded_at = Some(Utc::now());
        // A fold on a key that vanished is meaningless; drop it.
        let keys: HashSet<&str> = self.rows.iter().map(|row| row.key.as_str()).collect();
        self.collapsed.retain(|key| keys.contains(key.as_str()));
        let visible_len = self.visible().len();
        if !keep.is_some_and(|key| self.select_key(&key)) {
            self.selected = self.selected.min(visible_len.saturating_sub(1));
        }
    }

    pub fn memory(&self) -> TreeMemory {
        TreeMemory {
            selected_key: self.selected_row().map(|row| row.key.clone()),
            collapsed: self.collapsed.clone(),
        }
    }

    /// Restore folds and selection saved when the tree last closed.
    pub fn restore(&mut self, memory: &TreeMemory) {
        self.collapsed = memory.collapsed.clone();
        let keys: HashSet<&str> = self.rows.iter().map(|row| row.key.as_str()).collect();
        self.collapsed.retain(|key| keys.contains(key.as_str()));
        if let Some(key) = &memory.selected_key {
            self.select_key(key);
        }
    }

    /// Rows not yet fetched (`total_rows` is exact for the snapshot).
    pub fn unloaded_rows(&self) -> u64 {
        self.total_rows.saturating_sub(self.rows.len() as u64)
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

    fn collapse_all(&mut self) {
        let keep = self.selected_row().map(|row| row.key.clone());
        let parents: Vec<String> = (0..self.rows.len())
            .filter(|index| self.has_children(*index))
            .map(|index| self.rows[index].key.clone())
            .collect();
        self.collapsed.extend(parents);
        // Select the visible top-level ancestor of the previous selection.
        let mut target = keep;
        while let Some(key) = target.clone() {
            if self.select_key(&key) {
                return;
            }
            target = self
                .rows
                .iter()
                .find(|row| row.key == key)
                .and_then(|row| row.parent_key.clone());
        }
        self.selected = 0;
    }

    fn expand_all(&mut self) {
        let keep = self.selected_row().map(|row| row.key.clone());
        self.collapsed.clear();
        if let Some(key) = keep {
            self.select_key(&key);
        }
    }

    fn page_size(&self) -> usize {
        match self.viewport.get() {
            0 => DEFAULT_VIEWPORT,
            rows => rows,
        }
    }
}

fn count(value: Option<i64>) -> String {
    value.map_or_else(|| "?".into(), |n| n.to_string())
}

pub fn kind_tag(kind: ManagerTreeKindV1) -> &'static str {
    match kind {
        ManagerTreeKindV1::Global => "GLOBAL",
        ManagerTreeKindV1::Portfolio => "PORTFOLIO",
        ManagerTreeKindV1::Project => "PROJECT",
        ManagerTreeKindV1::Area => "AREA",
        ManagerTreeKindV1::Epic => "EPIC",
    }
}

/// Seat health as text: status, model, context fill and last activity.
pub fn seat_text(row: &ManagerTreeRowV1) -> String {
    match &row.seat {
        Some(seat) => {
            let mut text = format!("{:?}", seat.status);
            if let Some(model) = &seat.model {
                text.push_str(&format!(" {model}"));
            }
            text.push_str(&match seat.context_fill_pct {
                Some(pct) => format!(" ctx {pct:.0}%"),
                None => " ctx ?".into(),
            });
            text.push_str(&format!(" {}", seat.updated_at.format("%m-%d %H:%M")));
            text
        }
        None if row.focus_session_id.is_some() => "seat session missing".into(),
        None => "no seat".into(),
    }
}

/// Capabilities and allowance of a node grant.
pub fn grant_text(row: &ManagerTreeRowV1) -> Option<String> {
    let grant = row.grant.as_ref()?;
    let caps: Vec<String> = grant
        .capabilities
        .iter()
        .map(|c| format!("{c:?}"))
        .collect();
    let mut text = format!(
        "v{} caps {} · allow active {} sessions {} reports {}",
        grant.grant_version,
        if caps.is_empty() {
            "none".into()
        } else {
            caps.join("+")
        },
        grant.max_active_sessions,
        grant.max_created_sessions,
        grant.max_direct_reports
    );
    for reserved in &grant.reserved {
        text.push_str(&format!(
            " reserved {}={}",
            reserved.resource_kind, reserved.amount
        ));
    }
    Some(text)
}

/// #1412: the launches a row's manager may make now, as `model/effort` pairs
/// (provider shown only when it is not Claude). `None` when the row carries
/// no restriction to show.
pub fn launches_text(row: &ManagerTreeRowV1) -> Option<String> {
    if row.launches.is_empty() {
        return None;
    }
    let launches: Vec<String> = row
        .launches
        .iter()
        .map(|launch| {
            let model = launch
                .model
                .strip_prefix("claude-")
                .unwrap_or(&launch.model);
            let provider = match launch.provider {
                SessionProvider::Claude => String::new(),
                other => format!("{other:?}:"),
            };
            match &launch.effort {
                Some(effort) => format!("{provider}{model}/{effort}"),
                None => format!("{provider}{model}"),
            }
        })
        .collect();
    Some(format!("launches {}", launches.join(" ")))
}

/// Workload counts; `?` marks a count the daemon could not traverse.
pub fn load_text(row: &ManagerTreeRowV1) -> String {
    format!(
        "run {} reports {} esc {} dec {}",
        count(row.load.running_workers),
        count(row.load.direct_reports),
        count(row.load.pending_escalations),
        count(row.load.pending_decisions),
    )
}

/// One row as plain text (no indentation or fold marker).
pub fn row_text(row: &ManagerTreeRowV1) -> String {
    let mut parts = vec![format!("{} {}", kind_tag(row.kind), row.label)];
    parts.push(seat_text(row));
    if let Some(scope) = &row.scope {
        parts.push(scope.clone());
    }
    if let Some(grant) = grant_text(row) {
        parts.push(grant);
    }
    if let Some(launches) = launches_text(row) {
        parts.push(launches);
    }
    parts.push(load_text(row));
    if !row.complete {
        parts.push("children incomplete".into());
    }
    parts.join(" · ")
}

pub(super) fn state(app: &mut App) -> Option<&mut ManagerTreeState> {
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

/// The leaf session focused behind the overlay, as an appoint candidate.
pub(super) fn sync_candidate(app: &mut App) {
    let candidate = app.selected_session_state().map(|state| {
        let session = &state.session;
        Candidate {
            id: session.id,
            name: session
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or(&session.query)
                .chars()
                .take(48)
                .collect(),
            project_id: session.project_id,
            eligible: rsi_common::is_leaf_kind(session.session_kind)
                && !matches!(
                    session.status,
                    rsi_common::types::SessionStatus::Archived
                        | rsi_common::types::SessionStatus::Deleted
                ),
        }
    });
    if let Some(tree) = state(app) {
        tree.candidate = candidate;
    }
}

/// Which console Enter opens for a tree row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    /// A pre-#1236 daemon's global row: the `gm` console.
    Global,
    Node(ManagerNodeRefV1),
}

/// #1240: the manager node a row stands for (`None` for an Epic row).
#[must_use]
pub fn console_target(row: &ManagerTreeRowV1) -> Option<Console> {
    match row.kind {
        ManagerTreeKindV1::Global => Some(Console::Global),
        ManagerTreeKindV1::Portfolio => row
            .node_id
            .map(|node_id| Console::Node(ManagerNodeRefV1::Portfolio { node_id })),
        ManagerTreeKindV1::Project => row
            .project_id
            .map(|project_id| Console::Node(ManagerNodeRefV1::Project { project_id })),
        ManagerTreeKindV1::Area => row
            .node_id
            .map(|node_id| Console::Node(ManagerNodeRefV1::Area { node_id })),
        ManagerTreeKindV1::Epic => None,
    }
}

pub async fn open(app: &mut App) {
    match fetch(app, None, Vec::new(), AUTO_PAGES).await {
        Ok((rows, page)) => {
            let mut tree = ManagerTreeState::default();
            tree.install(rows, &page);
            if let Some(memory) = &app.manager_tree_memory {
                tree.restore(memory);
            }
            app.overlay = OverlayState::ManagerTree(Box::new(tree));
            sync_candidate(app);
        }
        Err(error) => app.notify_error(format!("Manager tree: {error}")),
    }
    app.mark_dirty();
}

/// Close the tree, remembering its selection and folds for the next open.
pub(super) fn close(app: &mut App) {
    if let Some(tree) = state(app) {
        let memory = tree.memory();
        app.manager_tree_memory = Some(memory);
    }
    app.overlay = OverlayState::None;
}

/// Reload from the start (a CAS-style refresh: the daemon snapshot is the
/// truth after a succession or rescope), keeping the selected row.
pub(super) async fn refresh(app: &mut App) -> bool {
    match fetch(app, None, Vec::new(), AUTO_PAGES).await {
        Ok((rows, page)) => {
            if let Some(tree) = state(app) {
                tree.install(rows, &page);
                tree.error = None;
            }
            true
        }
        Err(error) => {
            if let Some(tree) = state(app) {
                tree.error = Some(format!("Refresh failed: {error}"));
            }
            false
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
        Err(error) if error.contains("manager_tree_stale_cursor") => {
            refresh(app).await;
        }
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
                if let Some(tree) = state(app) {
                    tree.notice = Some(Notice {
                        text: format!("Session unavailable: {error}"),
                        tone: Tone::Error,
                    });
                }
                return;
            }
        }
    }
    close(app);
    app.open_session_in_current_pane(session_id);
}

fn set_notice(app: &mut App, text: impl Into<String>, tone: Tone) {
    if let Some(tree) = state(app) {
        tree.notice = Some(Notice {
            text: text.into(),
            tone,
        });
    }
}

pub async fn handle_key(app: &mut App, key: KeyEvent) {
    sync_candidate(app);
    let Some(tree) = state(app) else { return };
    if tree.modal.is_some() {
        actions::handle_modal_key(app, key).await;
        app.mark_dirty();
        return;
    }
    let visible = tree.visible();
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc if tree.move_source.is_some() => {
            tree.move_source = None;
            tree.notice = Some(Notice {
                text: "Move cancelled.".into(),
                tone: Tone::Info,
            });
        }
        KeyCode::Esc | KeyCode::Char('q') => close(app),
        KeyCode::Char('r') => {
            if refresh(app).await {
                set_notice(app, "Refreshed from the daemon.", Tone::Info);
            }
        }
        KeyCode::Char('n') => {
            if tree.next_after.is_some() {
                more(app).await;
            } else {
                tree.notice = Some(Notice {
                    text: "Every node is loaded.".into(),
                    tone: Tone::Info,
                });
            }
        }
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
                    tree.select_key(&key);
                }
            }
        }
        KeyCode::Char('l') | KeyCode::Right => {
            if let Some(row) = tree.selected_row() {
                let key = row.key.clone();
                tree.collapsed.remove(&key);
            }
        }
        KeyCode::Char(' ') => {
            if let Some(index) = tree.selected_index()
                && tree.has_children(index)
            {
                let key = tree.rows[index].key.clone();
                if !tree.collapsed.remove(&key) {
                    tree.collapsed.insert(key);
                }
            }
        }
        KeyCode::Char('H') => tree.collapse_all(),
        KeyCode::Char('L') => tree.expand_all(),
        KeyCode::PageDown | KeyCode::Char('d') if key.code == KeyCode::PageDown || ctrl => {
            let page = tree.page_size();
            tree.selected = (tree.selected + page).min(visible.len().saturating_sub(1));
            if tree.selected + 1 == visible.len() && tree.next_after.is_some() {
                more(app).await;
            }
        }
        KeyCode::PageUp | KeyCode::Char('u') if key.code == KeyCode::PageUp || ctrl => {
            let page = tree.page_size();
            tree.selected = tree.selected.saturating_sub(page);
        }
        KeyCode::Enter => {
            // #1240: a manager node row opens that node's console; an Epic
            // row jumps to its lead.
            let console = tree.selected_row().and_then(console_target);
            let target = tree.selected_row().and_then(|row| row.focus_session_id);
            match (console, target) {
                (Some(Console::Global), _) => {
                    close(app);
                    crate::overlay::global_manager_workspace::open(app).await;
                }
                (Some(Console::Node(node)), _) => {
                    close(app);
                    crate::overlay::global_manager_workspace::open_node(app, node).await;
                }
                (None, Some(id)) => jump(app, id).await,
                (None, None) => {
                    tree.notice = Some(Notice {
                        text: "This node has no seat session to open (o opens its project).".into(),
                        tone: Tone::Error,
                    });
                }
            }
        }
        KeyCode::Char('o') => {
            let project = tree.selected_row().and_then(|row| row.project_id);
            match project {
                Some(project_id) => {
                    close(app);
                    app.open_project_workspace(project_id);
                }
                None => {
                    tree.notice = Some(Notice {
                        text: "A manager row above project level has no single project to open."
                            .into(),
                        tone: Tone::Error,
                    });
                }
            }
        }
        KeyCode::Char('p') => actions::begin(app, TreeAction::Preview).await,
        KeyCode::Char('a') => actions::begin(app, TreeAction::Appoint).await,
        KeyCode::Char('e') => actions::begin(app, TreeAction::Edit).await,
        KeyCode::Char('x') => actions::begin(app, TreeAction::Revoke).await,
        KeyCode::Char('A') => actions::begin(app, TreeAction::Above).await,
        KeyCode::Char('m') => actions::begin(app, TreeAction::MoveUnder).await,
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
