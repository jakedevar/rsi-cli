//! The operator to-do panel (#1747 S2): the pure state behind the right-side
//! panel in session detail and the status-line count badge.
//!
//! The list itself lives in the daemon (`OperatorTodoList` / `Add` / `Close`,
//! operator-only RPCs). This module holds what the TUI adds on top of it:
//!
//! * the panel's open/closed choice and a selection cursor,
//! * the catch-up baseline: the time the operator last had the panel in view.
//!   Items created after it, and items an agent closed after it, are
//!   highlighted so a returning operator sees what changed. The baseline is
//!   fixed for a run (so highlights stay while the operator reads them) and
//!   only moves when the operator acknowledges.
//!
//! Everything here is synchronous and free of I/O so it can be unit tested;
//! the daemon calls live in `app/todo.rs` and the drawing in `ui/todo_panel.rs`.

use chrono::{DateTime, Utc};
use rsi_common::operator_todos::{
    OPERATOR_TODO_OPERATOR, OPERATOR_TODO_UNDO_WINDOW_SECS, OperatorTodoListResultV1,
    OperatorTodoStatusV1, OperatorTodoV1,
};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

/// Panel width in columns, border excluded.
pub const PANEL_WIDTH: u16 = 40;
/// Narrowest session-detail area that still has room for the panel; below it
/// the panel hides itself (the badge still shows the count).
pub const PANEL_MIN_AREA_WIDTH: u16 = 120;

/// Where an item's `link` points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoLink {
    /// `issue:#N`: an Issue by display number.
    Issue(u64),
    /// `session:<uuid>`: a session.
    Session(Uuid),
    /// An `http://` or `https://` URL.
    Url(String),
}

impl TodoLink {
    /// Parse an item's `link`; anything unrecognised is `None` (shown as text
    /// only, never opened).
    #[must_use]
    pub fn parse(link: &str) -> Option<Self> {
        let link = link.trim();
        if let Some(rest) = link.strip_prefix("issue:") {
            return rest.trim_start_matches('#').parse().ok().map(Self::Issue);
        }
        if let Some(rest) = link.strip_prefix("session:") {
            return rest.parse().ok().map(Self::Session);
        }
        if link.starts_with("https://") || link.starts_with("http://") {
            return Some(Self::Url(link.to_string()));
        }
        None
    }

    /// Short text for the item row.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Issue(number) => format!("issue #{number}"),
            Self::Session(id) => format!("session {}", &id.to_string()[..8]),
            Self::Url(url) => url.clone(),
        }
    }
}

/// The pop-up over one to-do item (#2950): what the operator can do with the
/// panel's selected item, and the y/n prompt that guards a close.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoPopupState {
    /// First visible row of the wrapped body.
    pub scroll: usize,
    /// A close waiting for `y`: done or dismissed. `None` shows the item.
    pub confirm: Option<OperatorTodoStatusV1>,
}

/// The operator's latest close, kept for [`OPERATOR_TODO_UNDO_WINDOW_SECS`] so
/// `<Space>du` can reopen it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    pub id: Uuid,
    pub title: String,
    pub status: OperatorTodoStatusV1,
    pub closed_at: DateTime<Utc>,
}

impl UndoEntry {
    /// Whole seconds of the window left at `now` (0 once it has passed).
    #[must_use]
    pub fn seconds_left(&self, now: DateTime<Utc>) -> i64 {
        (OPERATOR_TODO_UNDO_WINDOW_SECS - (now - self.closed_at).num_seconds()).max(0)
    }

    /// Whether the close can still be undone at `now`.
    #[must_use]
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        (now - self.closed_at).num_seconds() < OPERATOR_TODO_UNDO_WINDOW_SECS
    }
}

/// State of the operator to-do panel.
#[derive(Debug, Clone, Default)]
pub struct TodoPanel {
    /// The operator wants the panel shown (remembered across restarts).
    pub open: bool,
    /// The last time the panel was in view; persisted, advanced while visible.
    pub last_visit: Option<DateTime<Utc>>,
    /// Catch-up cutoff for this run: `last_visit` at start, or the last
    /// acknowledgement. `None` on a first run (nothing is "new" yet).
    baseline: Option<DateTime<Utc>>,
    open_items: Vec<OperatorTodoV1>,
    closed_items: Vec<OperatorTodoV1>,
    selected: Option<Uuid>,
    loaded: bool,
    undo: Option<UndoEntry>,
}

impl TodoPanel {
    /// A panel restored from persisted state.
    #[must_use]
    pub fn restored(open: bool, last_visit: Option<DateTime<Utc>>) -> Self {
        Self {
            open,
            last_visit,
            baseline: last_visit,
            ..Self::default()
        }
    }

    /// The `closed_since` to ask the daemon for.
    #[must_use]
    pub const fn closed_since(&self) -> Option<DateTime<Utc>> {
        self.baseline
    }

    /// Whether a list has been read from the daemon yet.
    #[must_use]
    pub const fn loaded(&self) -> bool {
        self.loaded
    }

    /// Replace the lists with a fresh daemon read. Open items are ordered
    /// most urgent first, then newest first; the selection follows its item
    /// and falls back to the first row.
    pub fn apply(&mut self, list: OperatorTodoListResultV1) {
        let mut open = list.open;
        sort_open(&mut open);
        self.open_items = open;
        self.closed_items = list.recently_closed;
        self.loaded = true;
        let keeps = self
            .selected
            .is_some_and(|id| self.open_items.iter().any(|item| item.id == id));
        if !keeps {
            self.selected = self.open_items.first().map(|item| item.id);
        }
    }

    /// Put a just-reopened item (the undo's result) into the open list without
    /// waiting for the next daemon read, and select it.
    pub fn restore_open(&mut self, item: OperatorTodoV1) {
        let id = item.id;
        if !self.open_items.iter().any(|open| open.id == id) {
            self.open_items.push(item);
            sort_open(&mut self.open_items);
        }
        self.selected = Some(id);
    }

    /// Forget everything read from the daemon (the connection was lost).
    pub fn clear(&mut self) {
        self.open_items.clear();
        self.closed_items.clear();
        self.selected = None;
        self.loaded = false;
    }

    /// Open items, most urgent then newest first.
    #[must_use]
    pub fn open_items(&self) -> &[OperatorTodoV1] {
        &self.open_items
    }

    /// Number of open items: the status-line badge.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.open_items.len()
    }

    /// Whether `item` was created after the catch-up baseline.
    #[must_use]
    pub fn is_new(&self, item: &OperatorTodoV1) -> bool {
        self.baseline
            .is_some_and(|baseline| item.created_at > baseline)
    }

    /// Open items added since the baseline.
    #[must_use]
    pub fn new_count(&self) -> usize {
        self.open_items
            .iter()
            .filter(|item| self.is_new(item))
            .count()
    }

    /// Items an agent closed since the baseline, newest first. What the
    /// operator closed themselves is not news.
    #[must_use]
    pub fn closed_since_visit(&self) -> Vec<&OperatorTodoV1> {
        let Some(baseline) = self.baseline else {
            return Vec::new();
        };
        self.closed_items
            .iter()
            .filter(|item| {
                item.closed_by.as_deref() != Some(OPERATOR_TODO_OPERATOR)
                    && item.closed_at.is_some_and(|closed| closed >= baseline)
            })
            .collect()
    }

    /// Anything for a returning operator to catch up on.
    #[must_use]
    pub fn has_catch_up(&self) -> bool {
        self.new_count() > 0 || !self.closed_since_visit().is_empty()
    }

    /// The selected open item.
    #[must_use]
    pub fn selected(&self) -> Option<&OperatorTodoV1> {
        let id = self.selected?;
        self.open_items.iter().find(|item| item.id == id)
    }

    /// Index of the selected open item.
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.open_items.iter().position(|item| item.id == id)
    }

    /// Move the cursor down one row (clamped).
    pub fn select_next(&mut self) {
        self.step(1);
    }

    /// Move the cursor up one row (clamped).
    pub fn select_prev(&mut self) {
        self.step(-1);
    }

    fn step(&mut self, delta: isize) {
        if self.open_items.is_empty() {
            self.selected = None;
            return;
        }
        let last = self.open_items.len() - 1;
        let next = match self.selected_index() {
            Some(index) => index.saturating_add_signed(delta).min(last),
            None => 0,
        };
        self.selected = Some(self.open_items[next].id);
    }

    /// Drop a just-closed item from the open list without waiting for the next
    /// daemon read, moving the cursor to its neighbour.
    pub fn remove_open(&mut self, id: Uuid) {
        let Some(index) = self.open_items.iter().position(|item| item.id == id) else {
            return;
        };
        self.open_items.remove(index);
        if self.selected == Some(id) {
            let next = index.min(self.open_items.len().saturating_sub(1));
            self.selected = self.open_items.get(next).map(|item| item.id);
        }
    }

    /// Remember the operator's close of `item` so it can be undone until the
    /// window passes. Only the latest close is undoable.
    pub fn record_close(
        &mut self,
        item: &OperatorTodoV1,
        status: OperatorTodoStatusV1,
        now: DateTime<Utc>,
    ) {
        self.undo = Some(UndoEntry {
            id: item.id,
            title: item.title.clone(),
            status,
            closed_at: now,
        });
    }

    /// The close that can still be undone at `now`.
    #[must_use]
    pub fn undo_available(&self, now: DateTime<Utc>) -> Option<&UndoEntry> {
        self.undo.as_ref().filter(|entry| entry.is_live(now))
    }

    /// Take the undoable close (leaving none), or `None` when there is none or
    /// its window has passed.
    pub fn take_undo(&mut self, now: DateTime<Utc>) -> Option<UndoEntry> {
        let entry = self.undo.take()?;
        entry.is_live(now).then_some(entry)
    }

    /// Forget a close whose window has passed; true when one was dropped (the
    /// hint that offered it must be redrawn).
    pub fn expire_undo(&mut self, now: DateTime<Utc>) -> bool {
        if self.undo.as_ref().is_some_and(|entry| !entry.is_live(now)) {
            self.undo = None;
            return true;
        }
        false
    }

    /// The operator has caught up: clear every highlight and start counting
    /// from `now`.
    pub fn acknowledge(&mut self, now: DateTime<Utc>) {
        self.baseline = Some(now);
        self.last_visit = Some(now);
        self.closed_items.clear();
    }

    /// The panel is in view at `now`: remember it for the next run's
    /// catch-up. This run's baseline does not move.
    pub fn touch_visit(&mut self, now: DateTime<Utc>) {
        self.last_visit = Some(now);
    }
}

/// Most urgent first, then newest first.
fn sort_open(items: &mut [OperatorTodoV1]) {
    items.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// The close request the "mark done" and "dismiss" keys send, or `None` with
/// nothing selected.
#[must_use]
pub fn close_target(
    panel: &TodoPanel,
    status: OperatorTodoStatusV1,
) -> Option<(Uuid, OperatorTodoStatusV1)> {
    if status == OperatorTodoStatusV1::Open {
        return None;
    }
    panel.selected().map(|item| (item.id, status))
}

/// Break `text` into rows of at most `width` cells for the pop-up: newlines
/// start a row, spaces are kept as typed and a long word is cut at the edge, so
/// a command reads exactly as it will paste. Never truncates.
#[must_use]
pub fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut row = String::new();
        let mut row_width = 0usize;
        for ch in line.chars() {
            let w = ch.width().unwrap_or(0);
            if row_width + w > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push(ch);
            row_width += w;
        }
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// `urgent`, `high`, `normal`, `low` for a priority 1-4.
#[must_use]
pub const fn priority_label(priority: u8) -> &'static str {
    match priority {
        1 => "urgent",
        2 => "high",
        3 => "normal",
        _ => "low",
    }
}

/// `3m`, `2h`, `5d`: how long ago `created` was.
#[must_use]
pub fn age_label(now: DateTime<Utc>, created: DateTime<Utc>) -> String {
    let seconds = (now - created).num_seconds().max(0);
    match seconds {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Where an item came from, for the row's last line: the operator, or the
/// seat that wrote it (`project_name` resolves a project scope).
#[must_use]
pub fn source_label(item: &OperatorTodoV1, project_name: Option<&str>) -> String {
    if item.scope == OPERATOR_TODO_OPERATOR {
        return "you".to_string();
    }
    let kind = item.scope.split(':').next().unwrap_or("manager");
    match (kind, project_name) {
        ("project", Some(name)) => name.to_string(),
        ("portfolio", _) => "portfolio".to_string(),
        (other, _) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 0, 0, 0).unwrap() + chrono::Duration::minutes(minutes)
    }

    fn item(name: &str, priority: u8, created: i64) -> OperatorTodoV1 {
        OperatorTodoV1 {
            id: Uuid::new_v4(),
            scope: "portfolio:global".into(),
            project_id: None,
            todo_key: name.into(),
            title: name.into(),
            how: format!("do {name}"),
            link: None,
            priority,
            added_by: "session:abc".into(),
            status: OperatorTodoStatusV1::Open,
            evidence: None,
            evidence_observed: false,
            closed_by: None,
            created_at: at(created),
            updated_at: at(created),
            closed_at: None,
        }
    }

    fn closed(name: &str, closed_at: i64, by: &str) -> OperatorTodoV1 {
        let mut closed = item(name, 3, 0);
        closed.status = OperatorTodoStatusV1::Done;
        closed.closed_at = Some(at(closed_at));
        closed.closed_by = Some(by.into());
        closed
    }

    fn list(open: Vec<OperatorTodoV1>, closed: Vec<OperatorTodoV1>) -> OperatorTodoListResultV1 {
        OperatorTodoListResultV1 {
            open,
            recently_closed: closed,
            truncated: false,
        }
    }

    #[test]
    fn open_items_are_most_urgent_then_newest_first() {
        let mut panel = TodoPanel::default();
        panel.apply(list(
            vec![item("old-p3", 3, 1), item("new-p3", 3, 9), item("p1", 1, 2)],
            vec![],
        ));
        let titles: Vec<_> = panel
            .open_items()
            .iter()
            .map(|i| i.title.as_str())
            .collect();
        assert_eq!(titles, ["p1", "new-p3", "old-p3"]);
        assert_eq!(panel.open_count(), 3);
        assert_eq!(panel.selected().map(|i| i.title.as_str()), Some("p1"));
    }

    #[test]
    fn cursor_is_clamped_and_follows_its_item_across_refreshes() {
        let mut panel = TodoPanel::default();
        let a = item("a", 1, 3);
        let b = item("b", 2, 2);
        let c = item("c", 3, 1);
        panel.apply(list(vec![a.clone(), b.clone(), c.clone()], vec![]));
        panel.select_prev();
        assert_eq!(panel.selected_index(), Some(0));
        panel.select_next();
        panel.select_next();
        panel.select_next();
        assert_eq!(panel.selected_index(), Some(2));
        // A new more urgent item arrives; the cursor stays on `c`.
        panel.apply(list(vec![item("z", 1, 9), a, b, c.clone()], vec![]));
        assert_eq!(panel.selected().map(|i| i.id), Some(c.id));
        // The selected item vanishes: fall back to the first row.
        panel.apply(list(vec![item("only", 2, 1)], vec![]));
        assert_eq!(panel.selected().map(|i| i.title.as_str()), Some("only"));
        panel.apply(list(vec![], vec![]));
        assert!(panel.selected().is_none());
        panel.select_next();
        assert!(panel.selected().is_none());
    }

    #[test]
    fn items_added_after_the_last_visit_are_new() {
        let mut panel = TodoPanel::restored(true, Some(at(10)));
        panel.apply(list(vec![item("old", 3, 5), item("fresh", 3, 20)], vec![]));
        let fresh = panel
            .open_items()
            .iter()
            .find(|i| i.title == "fresh")
            .unwrap();
        let old = panel
            .open_items()
            .iter()
            .find(|i| i.title == "old")
            .unwrap();
        assert!(panel.is_new(fresh));
        assert!(!panel.is_new(old));
        assert_eq!(panel.new_count(), 1);
        assert!(panel.has_catch_up());
    }

    #[test]
    fn a_first_run_has_nothing_new_and_nothing_closed() {
        let mut panel = TodoPanel::restored(true, None);
        panel.apply(list(
            vec![item("a", 3, 5)],
            vec![closed("b", 6, "session:x")],
        ));
        assert_eq!(panel.new_count(), 0);
        assert!(panel.closed_since_visit().is_empty());
        assert!(!panel.has_catch_up());
        assert_eq!(panel.closed_since(), None);
    }

    #[test]
    fn what_the_operator_closed_is_not_catch_up_news() {
        let mut panel = TodoPanel::restored(true, Some(at(10)));
        panel.apply(list(
            vec![],
            vec![
                closed("by-agent", 30, "session:abc"),
                closed("by-me", 31, OPERATOR_TODO_OPERATOR),
                closed("before", 5, "session:abc"),
            ],
        ));
        let titles: Vec<_> = panel
            .closed_since_visit()
            .iter()
            .map(|i| i.title.as_str())
            .collect();
        assert_eq!(titles, ["by-agent"]);
    }

    #[test]
    fn acknowledging_clears_every_highlight_and_moves_the_baseline() {
        let mut panel = TodoPanel::restored(true, Some(at(10)));
        panel.apply(list(
            vec![item("fresh", 3, 20)],
            vec![closed("c", 30, "session:a")],
        ));
        assert!(panel.has_catch_up());
        panel.acknowledge(at(40));
        assert!(!panel.has_catch_up());
        assert_eq!(panel.closed_since(), Some(at(40)));
        assert_eq!(panel.last_visit, Some(at(40)));
        // Items that arrive later are new again.
        panel.apply(list(vec![item("later", 3, 50)], vec![]));
        assert_eq!(panel.new_count(), 1);
    }

    #[test]
    fn touching_the_visit_does_not_move_this_runs_baseline() {
        let mut panel = TodoPanel::restored(true, Some(at(10)));
        panel.touch_visit(at(99));
        assert_eq!(panel.last_visit, Some(at(99)));
        assert_eq!(panel.closed_since(), Some(at(10)));
    }

    #[test]
    fn removing_the_selected_item_moves_the_cursor_to_its_neighbour() {
        let mut panel = TodoPanel::default();
        let a = item("a", 1, 3);
        let b = item("b", 2, 2);
        let c = item("c", 3, 1);
        panel.apply(list(vec![a.clone(), b.clone(), c.clone()], vec![]));
        panel.select_next();
        assert_eq!(panel.selected().map(|i| i.id), Some(b.id));
        panel.remove_open(b.id);
        assert_eq!(panel.selected().map(|i| i.id), Some(c.id));
        panel.remove_open(c.id);
        assert_eq!(panel.selected().map(|i| i.id), Some(a.id));
        panel.remove_open(a.id);
        assert!(panel.selected().is_none());
        assert_eq!(panel.open_count(), 0);
    }

    #[test]
    fn done_and_dismiss_target_the_selected_item_only() {
        let mut panel = TodoPanel::default();
        assert_eq!(close_target(&panel, OperatorTodoStatusV1::Done), None);
        let a = item("a", 1, 3);
        panel.apply(list(vec![a.clone()], vec![]));
        assert_eq!(
            close_target(&panel, OperatorTodoStatusV1::Done),
            Some((a.id, OperatorTodoStatusV1::Done))
        );
        assert_eq!(
            close_target(&panel, OperatorTodoStatusV1::Dismissed),
            Some((a.id, OperatorTodoStatusV1::Dismissed))
        );
        assert_eq!(close_target(&panel, OperatorTodoStatusV1::Open), None);
    }

    #[test]
    fn links_parse_by_kind_and_unknown_text_is_not_openable() {
        let session = Uuid::new_v4();
        assert_eq!(TodoLink::parse("issue:#1747"), Some(TodoLink::Issue(1747)));
        assert_eq!(TodoLink::parse("issue:12"), Some(TodoLink::Issue(12)));
        assert_eq!(
            TodoLink::parse(&format!("session:{session}")),
            Some(TodoLink::Session(session))
        );
        assert_eq!(
            TodoLink::parse("https://example.com/x"),
            Some(TodoLink::Url("https://example.com/x".into()))
        );
        assert_eq!(TodoLink::parse("file:///etc/passwd"), None);
        assert_eq!(TodoLink::parse("issue:abc"), None);
        assert_eq!(TodoLink::parse("session:nope"), None);
    }

    #[test]
    fn age_and_source_labels() {
        assert_eq!(age_label(at(0), at(0)), "now");
        assert_eq!(age_label(at(5), at(0)), "5m");
        assert_eq!(age_label(at(150), at(0)), "2h");
        assert_eq!(age_label(at(60 * 24 * 3 + 5), at(0)), "3d");
        assert_eq!(age_label(at(0), at(5)), "now");

        let mut it = item("a", 3, 0);
        assert_eq!(source_label(&it, None), "portfolio");
        it.scope = "project:abc".into();
        assert_eq!(source_label(&it, Some("rsi")), "rsi");
        assert_eq!(source_label(&it, None), "project");
        it.scope = OPERATOR_TODO_OPERATOR.into();
        assert_eq!(source_label(&it, None), "you");
    }

    #[test]
    fn the_latest_operator_close_is_undoable_only_inside_the_window() {
        let mut panel = TodoPanel::default();
        let a = item("a", 1, 3);
        panel.apply(list(vec![a.clone()], vec![]));
        assert!(panel.undo_available(at(0)).is_none());
        panel.record_close(&a, OperatorTodoStatusV1::Done, at(10));
        let entry = panel.undo_available(at(10)).expect("fresh close");
        assert_eq!(
            (entry.id, entry.status, entry.title.as_str()),
            (a.id, OperatorTodoStatusV1::Done, "a")
        );
        assert_eq!(entry.seconds_left(at(10)), OPERATOR_TODO_UNDO_WINDOW_SECS);
        // One second short of the window (60 s) is still live, the window itself is not.
        let almost = at(10) + chrono::Duration::seconds(OPERATOR_TODO_UNDO_WINDOW_SECS - 1);
        assert_eq!(
            panel.undo_available(almost).map(|e| e.seconds_left(almost)),
            Some(1)
        );
        let over = at(10) + chrono::Duration::seconds(OPERATOR_TODO_UNDO_WINDOW_SECS);
        assert!(panel.undo_available(over).is_none());
        assert!(panel.expire_undo(over));
        assert!(!panel.expire_undo(over), "expiry reports once");
    }

    #[test]
    fn a_newer_close_replaces_the_undo_and_taking_it_leaves_none() {
        let mut panel = TodoPanel::default();
        let a = item("a", 1, 3);
        let b = item("b", 2, 2);
        panel.record_close(&a, OperatorTodoStatusV1::Done, at(1));
        panel.record_close(&b, OperatorTodoStatusV1::Dismissed, at(2));
        let taken = panel.take_undo(at(2)).expect("undo b");
        assert_eq!(
            (taken.id, taken.status),
            (b.id, OperatorTodoStatusV1::Dismissed)
        );
        assert!(panel.take_undo(at(2)).is_none());
        // A late take yields nothing.
        panel.record_close(&a, OperatorTodoStatusV1::Done, at(0));
        let late = at(0) + chrono::Duration::seconds(OPERATOR_TODO_UNDO_WINDOW_SECS + 5);
        assert!(panel.take_undo(late).is_none());
    }

    #[test]
    fn a_restored_item_returns_to_its_place_and_is_selected() {
        let mut panel = TodoPanel::default();
        let a = item("a", 1, 3);
        let c = item("c", 3, 1);
        let b = item("b", 2, 2);
        panel.apply(list(vec![a.clone(), c.clone()], vec![]));
        panel.restore_open(b.clone());
        let ids: Vec<_> = panel.open_items().iter().map(|i| i.id).collect();
        assert_eq!(ids, [a.id, b.id, c.id]);
        assert_eq!(panel.selected().map(|i| i.id), Some(b.id));
        // Restoring twice does not duplicate.
        panel.restore_open(b);
        assert_eq!(panel.open_count(), 3);
    }

    #[test]
    fn hard_wrap_keeps_spaces_newlines_and_long_words_whole() {
        assert_eq!(hard_wrap("a  b", 10), ["a  b"]);
        assert_eq!(hard_wrap("one\ntwo", 10), ["one", "two"]);
        assert_eq!(hard_wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(hard_wrap("", 5), [""]);
        let how = "rsi grant restore --caps  v8   --scope portfolio:global --yes";
        let rows = hard_wrap(how, 16);
        assert!(rows.iter().all(|r| r.chars().count() <= 16));
        assert_eq!(rows.concat(), how);
    }

    #[test]
    fn priorities_read_as_words() {
        assert_eq!(
            [1, 2, 3, 4].map(priority_label),
            ["urgent", "high", "normal", "low"]
        );
    }
}
