//! Global Dashboard (#3317, slice C of #3314): the operator's RSI-wide
//! metrics view, `gd` or `:dashboard`. One full-screen overlay above every
//! project: a row per project (live agents by role and provider, tokens,
//! cost, Issue flow, landings, QA) with a TOTAL row, a detail band for the
//! selected row, and a side pane that talks to the top-level manager seat.
//!
//! It is a different feature from `gm` (the manager seat console): `gm` lists
//! seats; this view is metrics plus a line to the top manager. The numbers
//! come from one bounded read (`GetGlobalDashboard {window}`, #3316). The
//! pane reuses the gm console's conversation renderer and its send path
//! (`global_manager_workspace::{begin_typing, typing_key}`); the seat comes
//! from the grant (`GetGlobalManager`), and `n` starts one through the gm
//! console's appoint flow. Nothing here invents a new send path.
//!
//! Like `gm` it is an overlay rather than a pane because every tab is bound
//! to a project: it must look and work the same from any tab.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::global_dashboard::{
    DashboardAgents, DashboardIssues, DashboardLandings, DashboardModelUsage, DashboardUsage,
    DashboardWindow, GlobalDashboard, ProjectDashboard,
};
use rsi_common::types::SessionStatus;
use uuid::Uuid;

use crate::app::App;
use crate::overlay::global_manager_workspace as gm;
use crate::types::OverlayState;

/// While the dashboard is open, re-read the snapshot at most this often (a
/// bounded poll, never a busy loop).
pub const AUTO_REFRESH: Duration = Duration::from_secs(10);

/// At or above this inner width the conversation pane is shown by default.
pub const PANE_AUTO_MIN_WIDTH: u16 = 160;

/// Shown in the pane when no top-level manager seat is live.
pub const START_HINT: &str = "press n to start one";

/// What orders the project rows (the home project always leads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardSort {
    /// Live agents, then name (the default).
    #[default]
    Agents,
    /// Tokens in plus out.
    Tokens,
    Cost,
    /// Issues open plus in progress.
    Issues,
}

impl DashboardSort {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Agents => Self::Tokens,
            Self::Tokens => Self::Cost,
            Self::Cost => Self::Issues,
            Self::Issues => Self::Agents,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Tokens => "tokens",
            Self::Cost => "metered",
            Self::Issues => "Issues",
        }
    }
}

/// The window after `window` in the `w` cycle: 1h, 24h, 7d, back to 1h.
#[must_use]
pub const fn next_window(window: DashboardWindow) -> DashboardWindow {
    match window {
        DashboardWindow::Hour => DashboardWindow::Day,
        DashboardWindow::Day => DashboardWindow::Week,
        DashboardWindow::Week => DashboardWindow::Hour,
    }
}

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardFocus {
    /// The project table.
    #[default]
    Table,
    /// The conversation transcript (j/k scroll).
    Conversation,
    /// The conversation's input bar (typing).
    Input,
}

/// The top-level manager seat, from the global manager grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TopSeat {
    /// Not read yet.
    #[default]
    Unknown,
    /// No grant, a revoked grant, or a seat session that is gone.
    Absent,
    /// The seat's session.
    Seat(Uuid),
}

#[derive(Debug, Default)]
pub struct GlobalDashboardState {
    /// `None` until the first snapshot arrives.
    pub snapshot: Option<GlobalDashboard>,
    pub window: DashboardWindow,
    pub sort: DashboardSort,
    /// Index into the display rows: the sorted projects, then TOTAL.
    pub selected: usize,
    /// The last refresh failure; the previous snapshot stays on screen.
    pub error: Option<String>,
    pub notice: Option<String>,
    pub loaded_at: Option<DateTime<Utc>>,
    pub last_attempt: Option<Instant>,
    pub focus: DashboardFocus,
    /// `t`: `Some(true)` forces the pane, `Some(false)` hides it, `None` shows
    /// it when the terminal is wide enough.
    pub pane_override: Option<bool>,
    pub seat: TopSeat,
    /// A `g` was pressed: `m` next opens the manager workspace (`gm`).
    pub pending_g: bool,
    /// The inner width of the last frame drawn, which decides whether `t`
    /// hides or shows the pane.
    pub inner_width: u16,
}

/// Which pieces of a row the table renders; one value per column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Project,
    Pm,
    Agents,
    Managers,
    Leads,
    Workers,
    Reviewers,
    Providers,
    TokensIn,
    TokensOut,
    Cost,
    Open,
    InProgress,
    Created,
    Closed,
    Requests,
    Landings,
    Qa,
}

impl Col {
    const ALL: [Self; 18] = [
        Self::Project,
        Self::Pm,
        Self::Agents,
        Self::Managers,
        Self::Leads,
        Self::Workers,
        Self::Reviewers,
        Self::Providers,
        Self::TokensIn,
        Self::TokensOut,
        Self::Cost,
        Self::Open,
        Self::InProgress,
        Self::Created,
        Self::Closed,
        Self::Requests,
        Self::Landings,
        Self::Qa,
    ];

    /// Columns dropped first when the table is narrow, first to last.
    const DROP_ORDER: [Self; 14] = [
        Self::Reviewers,
        Self::Workers,
        Self::Leads,
        Self::Managers,
        Self::Providers,
        Self::Landings,
        Self::Closed,
        Self::Created,
        Self::Pm,
        Self::Requests,
        Self::Qa,
        Self::InProgress,
        Self::TokensOut,
        Self::TokensIn,
    ];

    #[must_use]
    pub const fn header(self, window: DashboardWindow) -> &'static str {
        match self {
            Self::Project => "PROJECT",
            Self::Pm => "PM",
            Self::Agents => "AGT",
            Self::Managers => "MGR",
            Self::Leads => "LEAD",
            Self::Workers => "WRK",
            Self::Reviewers => "REV",
            Self::Providers => "PROVIDERS",
            Self::TokensIn => "TOK IN",
            Self::TokensOut => "TOK OUT",
            Self::Cost => "METERED",
            Self::Open => "OPEN",
            Self::InProgress => "WIP",
            Self::Created => match window {
                DashboardWindow::Hour => "NEW1H",
                DashboardWindow::Day => "NEW24",
                DashboardWindow::Week => "NEW7D",
            },
            Self::Closed => match window {
                DashboardWindow::Hour => "DON1H",
                DashboardWindow::Day => "DON24",
                DashboardWindow::Week => "DON7D",
            },
            Self::Requests => "REQ",
            Self::Landings => "LAND",
            Self::Qa => "QA",
        }
    }

    /// Fixed width; `Project` is flexible and reports its minimum.
    #[must_use]
    pub const fn width(self) -> usize {
        match self {
            Self::Project => 14,
            Self::Pm => 2,
            Self::Agents | Self::Open | Self::InProgress | Self::Landings => 4,
            Self::Managers | Self::Workers | Self::Reviewers | Self::Requests => 3,
            Self::Leads => 4,
            Self::Providers => 20,
            Self::TokensIn | Self::Qa => 6,
            Self::TokensOut | Self::Cost => 7,
            Self::Created | Self::Closed => 5,
        }
    }

    #[must_use]
    pub const fn right_aligned(self) -> bool {
        !matches!(self, Self::Project | Self::Providers | Self::Pm | Self::Qa)
    }
}

/// The widest the project column grows.
const PROJECT_MAX_WIDTH: usize = 30;

/// The columns that fit `width` cells (a rail and a gap lead each row), each
/// with its width. Narrow tables drop the least important columns first; the
/// project column takes the spare room.
#[must_use]
pub fn layout(width: usize) -> Vec<(Col, usize)> {
    let mut cols: Vec<Col> = Col::ALL.to_vec();
    let used = |cols: &[Col]| {
        // The rail and a gap, then one gap between columns.
        2 + cols.iter().map(|c| c.width()).sum::<usize>() + cols.len().saturating_sub(1)
    };
    for drop in Col::DROP_ORDER {
        if used(&cols) <= width {
            break;
        }
        cols.retain(|c| *c != drop);
    }
    let spare = width.saturating_sub(used(&cols));
    cols.into_iter()
        .map(|c| {
            if c == Col::Project {
                (c, (Col::Project.width() + spare).min(PROJECT_MAX_WIDTH))
            } else {
                (c, c.width())
            }
        })
        .collect()
}

/// One table row: a project, or the TOTAL row.
#[derive(Debug, Clone, Copy)]
pub struct RowView<'a> {
    pub name: &'a str,
    pub is_home: bool,
    pub is_total: bool,
    /// The PM seat's session status, when the project has one.
    pub pm_status: Option<&'a str>,
    pub agents: &'a DashboardAgents,
    pub usage: &'a DashboardUsage,
    pub issues: &'a DashboardIssues,
    pub landings: &'a DashboardLandings,
    pub red_qa: u32,
    pub green_age_secs: Option<i64>,
    pub has_rolling: bool,
}

impl<'a> RowView<'a> {
    fn project(project: &'a ProjectDashboard) -> Self {
        Self {
            name: &project.name,
            is_home: project.is_home,
            is_total: false,
            pm_status: project.pm_seat.as_ref().map(|seat| seat.status.as_str()),
            agents: &project.agents,
            usage: &project.usage,
            issues: &project.issues,
            landings: &project.landings,
            red_qa: project.rolling.as_ref().map_or(0, |r| r.open_qa_issues),
            green_age_secs: project.rolling.as_ref().and_then(|r| r.qa_green_age_secs),
            has_rolling: project.rolling.is_some(),
        }
    }
}

/// Compact count: `812`, `1.2k`, `340k`, `1.2M`, `12M`, `1.2B`.
#[must_use]
pub fn fmt_count(value: u64) -> String {
    fn scaled(value: u64, unit: f64, suffix: &str) -> String {
        let n = value as f64 / unit;
        if n < 10.0 {
            format!("{n:.1}{suffix}")
        } else {
            format!("{n:.0}{suffix}")
        }
    }
    match value {
        0..=999 => value.to_string(),
        1_000..=999_499 => scaled(value, 1e3, "k"),
        999_500..=999_499_999 => scaled(value, 1e6, "M"),
        _ => scaled(value, 1e9, "B"),
    }
}

/// Dollars: `$0`, `$3.20`, `$123`, `$1.2k`.
#[must_use]
pub fn fmt_cost(usd: f64) -> String {
    if usd <= 0.0 {
        "$0".to_string()
    } else if usd < 10.0 {
        format!("${usd:.2}")
    } else if usd < 1000.0 {
        format!("${usd:.0}")
    } else {
        format!("${:.1}k", usd / 1000.0)
    }
}

/// `45s`, `12m`, `3h`, `2d`.
#[must_use]
pub fn fmt_age(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// The top providers, `Claude 4 · Codex 2`, busiest first; `+n` when more
/// exist than `limit`.
#[must_use]
pub fn providers_text(agents: &DashboardAgents, limit: usize) -> String {
    let mut providers: Vec<(&str, u64)> = agents
        .by_provider
        .iter()
        .filter(|p| p.count > 0)
        .map(|p| (p.provider.as_str(), p.count))
        .collect();
    providers.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    if providers.is_empty() {
        return "-".to_string();
    }
    let extra = providers.len().saturating_sub(limit);
    let mut text = providers
        .iter()
        .take(limit)
        .map(|(name, count)| format!("{name} {count}"))
        .collect::<Vec<_>>()
        .join(" · ");
    if extra > 0 {
        text.push_str(&format!(" +{extra}"));
    }
    text
}

/// The QA cell: a red count when `qa` Issues are open, else the age of the
/// last full-library green, else `-`.
#[must_use]
pub fn qa_text(row: &RowView<'_>) -> String {
    if row.red_qa > 0 {
        format!("{} red", row.red_qa)
    } else if let Some(age) = row.green_age_secs {
        fmt_age(age)
    } else {
        "-".to_string()
    }
}

/// The PM seat glyph source: `Some(true)` live, `Some(false)` stopped.
#[must_use]
pub fn pm_glyph(status: Option<&str>) -> &'static str {
    match status {
        None => "○",
        Some("Starting" | "Running") => "●",
        Some("WaitingApproval") => crate::ui::glyphs::NEEDS_YOU,
        Some("Failed" | "Interrupted") => crate::ui::glyphs::FAILED,
        Some(_) => crate::ui::glyphs::DONE,
    }
}

/// The text of one cell, untruncated and unpadded.
#[must_use]
pub fn cell_text(col: Col, row: &RowView<'_>) -> String {
    match col {
        Col::Project => {
            if row.is_home {
                format!("{} (home)", row.name)
            } else {
                row.name.to_string()
            }
        }
        Col::Pm => {
            if row.is_total {
                String::new()
            } else {
                pm_glyph(row.pm_status).to_string()
            }
        }
        Col::Agents => row.agents.total.to_string(),
        Col::Managers => row.agents.managers.to_string(),
        Col::Leads => row.agents.leads.to_string(),
        Col::Workers => row.agents.workers.to_string(),
        Col::Reviewers => row.agents.reviewers.to_string(),
        Col::Providers => providers_text(row.agents, 2),
        Col::TokensIn => fmt_count(row.usage.input_tokens),
        Col::TokensOut => fmt_count(row.usage.output_tokens),
        Col::Cost => {
            if row.usage.estimated_cost_usd == 0.0 {
                "—".into()
            } else {
                fmt_cost(row.usage.estimated_cost_usd)
            }
        }
        Col::Open => row.issues.open.to_string(),
        Col::InProgress => row.issues.in_progress.to_string(),
        Col::Created => row.issues.created_in_window.to_string(),
        Col::Closed => row.issues.closed_in_window.to_string(),
        Col::Requests => row.issues.open_operator_requests.to_string(),
        Col::Landings => row.landings.published.to_string(),
        Col::Qa => {
            if row.is_total {
                if row.red_qa > 0 {
                    format!("{} red", row.red_qa)
                } else {
                    String::new()
                }
            } else if row.has_rolling {
                qa_text(row)
            } else {
                "-".to_string()
            }
        }
    }
}

/// `text` cut to `width` characters (an ellipsis marks a cut) and padded.
#[must_use]
pub fn fit(text: &str, width: usize, right: bool) -> String {
    let count = text.chars().count();
    let text: String = if count > width {
        if width == 0 {
            String::new()
        } else {
            let mut cut: String = text.chars().take(width - 1).collect();
            cut.push('…');
            cut
        }
    } else {
        text.to_string()
    };
    if right {
        format!("{text:>width$}")
    } else {
        format!("{text:<width$}")
    }
}

fn model_usage_line(entry: &DashboardModelUsage) -> String {
    format!(
        "{} {} {} in {} out {}",
        entry.provider,
        entry.model,
        fmt_count(entry.usage.input_tokens),
        fmt_count(entry.usage.output_tokens),
        fmt_cost(entry.usage.estimated_cost_usd)
    )
}

/// `items` joined by ` · `, at most `limit`, with a `+n more` tail.
fn joined(items: Vec<String>, limit: usize) -> String {
    let extra = items.len().saturating_sub(limit);
    let mut text = items
        .into_iter()
        .take(limit)
        .collect::<Vec<_>>()
        .join(" · ");
    if extra > 0 {
        text.push_str(&format!(" · +{extra} more"));
    }
    text
}

impl GlobalDashboardState {
    /// The projects in display order: the home project first, then by the
    /// chosen sort (largest first), then by name.
    #[must_use]
    pub fn sorted_projects(&self) -> Vec<&ProjectDashboard> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut projects: Vec<&ProjectDashboard> = snapshot.projects.iter().collect();
        let sort = self.sort;
        let key = move |p: &ProjectDashboard| -> f64 {
            match sort {
                DashboardSort::Agents => p.agents.total as f64,
                DashboardSort::Tokens => (p.usage.input_tokens + p.usage.output_tokens) as f64,
                DashboardSort::Cost => p.usage.estimated_cost_usd,
                DashboardSort::Issues => (p.issues.open + p.issues.in_progress) as f64,
            }
        };
        projects.sort_by(|a, b| {
            b.is_home
                .cmp(&a.is_home)
                .then_with(|| key(b).total_cmp(&key(a)))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });
        projects
    }

    /// Rows shown: the sorted projects, then TOTAL.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.projects.len() + 1)
    }

    /// The selected project; `None` on the TOTAL row.
    #[must_use]
    pub fn selected_project(&self) -> Option<&ProjectDashboard> {
        self.sorted_projects().get(self.selected).copied()
    }

    /// Whether the TOTAL row is selected.
    #[must_use]
    pub fn total_selected(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| self.selected == snapshot.projects.len())
    }

    /// Install a snapshot, keeping the selection on the same project.
    pub fn install(&mut self, snapshot: GlobalDashboard, now: DateTime<Utc>) {
        let keep = self.selected_project().map(|p| p.id);
        let keep_total = self.total_selected();
        self.snapshot = Some(snapshot);
        self.loaded_at = Some(now);
        self.error = None;
        self.reselect(keep, keep_total);
    }

    /// Cycle the sort, keeping the selection on the same project.
    pub fn cycle_sort(&mut self) {
        let keep = self.selected_project().map(|p| p.id);
        let keep_total = self.total_selected();
        self.sort = self.sort.next();
        self.reselect(keep, keep_total);
    }

    fn reselect(&mut self, keep: Option<Uuid>, keep_total: bool) {
        let rows = self.row_count();
        if keep_total && rows > 0 {
            self.selected = rows - 1;
            return;
        }
        if let Some(id) = keep
            && let Some(index) = self.sorted_projects().iter().position(|p| p.id == id)
        {
            self.selected = index;
            return;
        }
        self.selected = self.selected.min(rows.saturating_sub(1));
    }

    /// The table rows in display order, TOTAL last.
    pub fn rows(&self) -> Vec<RowView<'_>> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut rows: Vec<RowView<'_>> = self
            .sorted_projects()
            .into_iter()
            .map(RowView::project)
            .collect();
        let red_total: u32 = snapshot
            .projects
            .iter()
            .filter_map(|p| p.rolling.as_ref())
            .map(|r| r.open_qa_issues)
            .sum();
        rows.push(RowView {
            name: "TOTAL",
            is_home: false,
            is_total: true,
            pm_status: None,
            agents: &snapshot.totals.agents,
            usage: &snapshot.totals.usage,
            issues: &snapshot.totals.issues,
            landings: &snapshot.totals.landings,
            red_qa: red_total,
            green_age_secs: None,
            has_rolling: false,
        });
        rows
    }

    /// Whether the RPC reported a capped (lower-bound) snapshot.
    #[must_use]
    pub fn incomplete(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.totals.incomplete
                || snapshot.projects_incomplete
                || !snapshot.incomplete_reasons.is_empty()
        })
    }

    /// The detail band for the selected row: agents by provider × model
    /// (× effort), tokens and cost by provider × model, and Issue flow.
    #[must_use]
    pub fn detail_lines(&self) -> Vec<String> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let window = snapshot.window.as_str();
        let (name, agents, usage_by_model, issues, landings, extra) =
            if let Some(project) = self.selected_project() {
                let seat = project.pm_seat.as_ref().map_or_else(
                    || "no PM seat".to_string(),
                    |seat| format!("PM seat {}", seat.status),
                );
                let home = if project.is_home {
                    " · home project"
                } else {
                    ""
                };
                (
                    format!("{}{home} · {seat}", project.name),
                    &project.agents,
                    &project.usage_by_model,
                    &project.issues,
                    &project.landings,
                    project.rolling.as_ref().map(|rolling| {
                        match (rolling.open_qa_issues, rolling.qa_green_age_secs) {
                            (0, Some(age)) => format!("QA green {} ago", fmt_age(age)),
                            (0, None) => "QA no full-library green yet".to_string(),
                            (n, _) => format!("QA {n} red"),
                        }
                    }),
                )
            } else {
                let totals = &snapshot.totals;
                let mut extra = None;
                if !snapshot.incomplete_reasons.is_empty() {
                    extra = Some(format!(
                        "Incomplete (lower bound): {}",
                        snapshot.incomplete_reasons.join(", ")
                    ));
                }
                (
                    "TOTAL · every project".to_string(),
                    &totals.agents,
                    &totals.usage_by_model,
                    &totals.issues,
                    &totals.landings,
                    extra,
                )
            };
        let mut lines = vec![name];
        let mut roles = format!(
            "Agents {} live · {} mgr · {} lead · {} wrk · {} rev",
            agents.total, agents.managers, agents.leads, agents.workers, agents.reviewers
        );
        if agents.other > 0 {
            roles.push_str(&format!(" · {} other", agents.other));
        }
        lines.push(roles);
        let by_model: Vec<String> = agents
            .by_model
            .iter()
            .filter(|g| g.count > 0)
            .map(|g| match g.effort.as_deref().filter(|e| !e.is_empty()) {
                Some(effort) => format!("{} {} {} ×{}", g.provider, g.model, effort, g.count),
                None => format!("{} {} ×{}", g.provider, g.model, g.count),
            })
            .collect();
        if by_model.is_empty() {
            lines.push("By model: no live agents".to_string());
        } else {
            lines.push(format!("By model: {}", joined(by_model, 6)));
        }
        let usage: Vec<String> = usage_by_model.iter().map(model_usage_line).collect();
        if usage.is_empty() {
            lines.push(format!("Usage {window}: none"));
        } else {
            lines.push(format!("Usage {window}: {}", joined(usage, 4)));
        }
        lines.push(format!(
            "Issues: {} open · {} in progress · {} created · {} closed in {window} · {} operator request{} · landings {} published, {} refused",
            issues.open,
            issues.in_progress,
            issues.created_in_window,
            issues.closed_in_window,
            issues.open_operator_requests,
            if issues.open_operator_requests == 1 { "" } else { "s" },
            landings.published,
            landings.refused,
        ));
        if let Some(extra) = extra {
            lines.push(extra);
        }
        lines
    }

    /// The pane is shown when forced on, or by default at a wide terminal.
    #[must_use]
    pub fn pane_visible(&self, inner_width: u16) -> bool {
        self.pane_override
            .unwrap_or(inner_width >= PANE_AUTO_MIN_WIDTH)
    }
}

/// The age of a snapshot, `12s ago`.
#[must_use]
pub fn snapshot_age(as_of: DateTime<Utc>, now: DateTime<Utc>) -> String {
    format!("{} ago", fmt_age((now - as_of).num_seconds()))
}

fn state(app: &mut App) -> Option<&mut GlobalDashboardState> {
    match &mut app.overlay {
        OverlayState::GlobalDashboard(state) => Some(state),
        _ => None,
    }
}

#[must_use]
pub fn is_open(app: &App) -> bool {
    matches!(app.overlay, OverlayState::GlobalDashboard(..))
}

/// The dashboard owns typed text while its input bar is active, so the overlay
/// leader must not take Space.
#[must_use]
pub fn owns_text_entry(state: &GlobalDashboardState) -> bool {
    state.focus == DashboardFocus::Input
}

/// The seat session the pane shows: the grant's seat while its session is
/// still there and not archived or deleted.
#[must_use]
pub fn live_seat(app: &App) -> Option<Uuid> {
    let OverlayState::GlobalDashboard(state) = &app.overlay else {
        return None;
    };
    let TopSeat::Seat(id) = state.seat else {
        return None;
    };
    let status = app.sessions.get(&id).map(|s| s.session.status)?;
    (!matches!(status, SessionStatus::Archived | SessionStatus::Deleted)).then_some(id)
}

/// The session the open dashboard's pane shows, when the pane is visible.
#[must_use]
pub fn open_conversation_session(app: &App) -> Option<Uuid> {
    live_seat(app)
}

/// Open the dashboard above whatever overlay or pane is showing, or refresh
/// it when it is already open (`gd` twice reloads).
pub async fn open(app: &mut App) {
    if !is_open(app) {
        app.overlay = OverlayState::GlobalDashboard(Box::default());
    }
    refresh(app).await;
    app.mark_dirty();
}

/// Re-read the snapshot and the top-level seat. A failure keeps the previous
/// snapshot on screen with an actionable error.
pub async fn refresh(app: &mut App) {
    let window = if let Some(dashboard) = state(app) {
        dashboard.last_attempt = Some(Instant::now());
        dashboard.window
    } else {
        return;
    };
    let result = app.client.get_global_dashboard(window).await;
    let grant = app.client.get_global_manager().await;
    let Some(dashboard) = state(app) else { return };
    match result {
        Ok(snapshot) => dashboard.install(snapshot, Utc::now()),
        Err(error) => {
            let stale = if dashboard.snapshot.is_some() {
                " (showing the last snapshot)"
            } else {
                ""
            };
            dashboard.error = Some(format!("Refresh failed: {error}{stale}; press r to retry"));
        }
    }
    // A failed grant read keeps the seat we already knew.
    if let Ok(grant) = grant {
        dashboard.seat = match grant {
            Some(grant) if grant.state == "active" => TopSeat::Seat(grant.seat_session_id),
            _ => TopSeat::Absent,
        };
    }
    ensure_conversation(app).await;
    app.mark_dirty();
}

/// Event-loop tick: re-read the snapshot every `AUTO_REFRESH` while open.
pub async fn tick(app: &mut App) {
    let due = match &app.overlay {
        OverlayState::GlobalDashboard(state) => state
            .last_attempt
            .is_none_or(|at| at.elapsed() >= AUTO_REFRESH),
        _ => false,
    };
    if due {
        refresh(app).await;
    }
}

/// Make sure the seat's session is cached and its conversation is loading.
/// Live events then arrive on the shared notification stream.
pub async fn ensure_conversation(app: &mut App) {
    let Some(TopSeat::Seat(session_id)) = state(app).map(|d| d.seat) else {
        return;
    };
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
                if let Some(dashboard) = state(app) {
                    dashboard.seat = TopSeat::Absent;
                    dashboard.error = Some(format!(
                        "Top-level manager session unavailable: {error}; {START_HINT}"
                    ));
                }
                return;
            }
        }
    }
    app.trigger_focus_fetch_if_needed(session_id);
    app.mark_dirty();
}

/// Close the dashboard on the selected project's tab and session list.
fn open_project(app: &mut App) {
    let Some(dashboard) = state(app) else { return };
    let Some(project_id) = dashboard.selected_project().map(|p| p.id) else {
        dashboard.error = Some("TOTAL is not a project; select a project row to open it".into());
        return;
    };
    if !app.projects.iter().any(|project| project.id == project_id) {
        if let Some(dashboard) = state(app) {
            dashboard.error = Some("That project no longer exists; press r to refresh".into());
        }
        return;
    }
    gm::jump_to_project(app, project_id);
}

/// `i`: type to the top-level manager here (showing the pane first).
fn start_typing(app: &mut App) {
    let Some(session_id) = live_seat(app) else {
        if let Some(dashboard) = state(app) {
            dashboard.error = Some(format!(
                "No top-level manager is live; {START_HINT} (it launches and appoints one)"
            ));
        }
        return;
    };
    match gm::begin_typing(app, session_id) {
        Ok(()) => {
            if let Some(dashboard) = state(app) {
                dashboard.pane_override = Some(true);
                dashboard.focus = DashboardFocus::Input;
                dashboard.error = None;
            }
        }
        Err(error) => {
            if let Some(dashboard) = state(app) {
                dashboard.error = Some(error);
            }
        }
    }
}

/// `n`: start a top-level manager through the gm console's appoint flow,
/// hosted in the home project when the snapshot names one.
async fn start_seat(app: &mut App) {
    let live = live_seat(app).is_some();
    let Some(dashboard) = state(app) else { return };
    if live {
        dashboard.notice = Some(
            "A top-level manager is already live; press i to type to it, or gm to replace it"
                .into(),
        );
        return;
    }
    let host = dashboard
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.projects.iter().find(|p| p.is_home))
        .map(|p| p.id);
    gm::open_appoint(app, host).await;
}

fn toggle_pane(app: &mut App) {
    let Some(dashboard) = state(app) else { return };
    let shown = dashboard.pane_visible(dashboard.inner_width);
    dashboard.pane_override = Some(!shown);
    if shown && dashboard.focus != DashboardFocus::Table {
        dashboard.focus = DashboardFocus::Table;
    }
}

/// Keys while the conversation transcript has focus.
fn handle_conversation_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers == KeyModifiers::CONTROL;
    let session = live_seat(app);
    let half = |app: &App, id: Option<Uuid>| {
        id.and_then(|id| app.sessions.get(&id))
            .map_or(10, |s| (s.last_viewport_height / 2).max(1))
    };
    let scroll = |app: &mut App, direction: i32, lines: usize| {
        if let Some(id) = session {
            gm::scroll_session(app, id, direction, lines);
        }
    };
    match key.code {
        KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab => {
            if let Some(dashboard) = state(app) {
                dashboard.focus = DashboardFocus::Table;
            }
        }
        KeyCode::Char('i') if !ctrl => start_typing(app),
        KeyCode::Char('j') | KeyCode::Down => scroll(app, 1, 3),
        KeyCode::Char('k') | KeyCode::Up => scroll(app, -1, 3),
        KeyCode::Char('d') if ctrl => {
            let lines = half(app, session);
            scroll(app, 1, lines);
        }
        KeyCode::Char('u') if ctrl => {
            let lines = half(app, session);
            scroll(app, -1, lines);
        }
        KeyCode::PageDown => {
            let lines = half(app, session) * 2;
            scroll(app, 1, lines);
        }
        KeyCode::PageUp => {
            let lines = half(app, session) * 2;
            scroll(app, -1, lines);
        }
        KeyCode::Char('g') => follow_tail(app, session, false),
        KeyCode::Char('G') => follow_tail(app, session, true),
        KeyCode::Char('?') => crate::overlay::keybindings_help::open_contextual_help(app),
        _ => {}
    }
}

fn follow_tail(app: &mut App, session_id: Option<Uuid>, follow: bool) {
    let Some(session) = session_id.and_then(|id| app.sessions.get_mut(&id)) else {
        return;
    };
    session.follow_tail = follow;
    session.follow_tail_hold = false;
    if !follow {
        session.scroll_offset = 0;
        session.current_event_index = (!session.events.is_empty()).then_some(0);
    }
}

/// Keys while typing to the top-level manager: the gm console's typing path.
async fn handle_input_key(app: &mut App, key: KeyEvent) {
    let Some(session_id) = live_seat(app) else {
        if let Some(dashboard) = state(app) {
            dashboard.focus = DashboardFocus::Table;
        }
        return;
    };
    if gm::typing_key(app, session_id, key).await
        && let Some(dashboard) = state(app)
    {
        dashboard.focus = DashboardFocus::Table;
    }
}

pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(dashboard) = state(app) else { return };
    match dashboard.focus {
        DashboardFocus::Input => {
            handle_input_key(app, key).await;
            app.mark_dirty();
            return;
        }
        DashboardFocus::Conversation => {
            handle_conversation_key(app, key);
            app.mark_dirty();
            return;
        }
        DashboardFocus::Table => {}
    }
    let pending_g = std::mem::take(&mut dashboard.pending_g);
    let len = dashboard.row_count();
    if pending_g && key.code == KeyCode::Char('m') && key.modifiers.is_empty() {
        gm::open(app).await;
        app.mark_dirty();
        return;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
        KeyCode::Char('r') => refresh(app).await,
        KeyCode::Enter | KeyCode::Char('l') => open_project(app),
        KeyCode::Char('w') => {
            dashboard.window = next_window(dashboard.window);
            dashboard.error = None;
            refresh(app).await;
        }
        KeyCode::Char('s') => dashboard.cycle_sort(),
        KeyCode::Char('t') => toggle_pane(app),
        KeyCode::Char('i') => start_typing(app),
        KeyCode::Char('n') => start_seat(app).await,
        KeyCode::Tab => {
            if live_seat(app).is_some() {
                if let Some(dashboard) = state(app) {
                    dashboard.pane_override = Some(true);
                    dashboard.focus = DashboardFocus::Conversation;
                }
            } else if let Some(dashboard) = state(app) {
                dashboard.error = Some(format!(
                    "No top-level manager is live; {START_HINT} (it launches and appoints one)"
                ));
            }
        }
        KeyCode::Char(':') => crate::overlay::command_palette::open_command_palette(app),
        KeyCode::Char('?') => crate::overlay::keybindings_help::open_contextual_help(app),
        _ => {
            let mut selected = dashboard.selected;
            if key.code == KeyCode::Char('g') {
                dashboard.pending_g = true;
            }
            if crate::overlay::list::handle_list_nav_key(&mut selected, len, &key)
                && selected != dashboard.selected
            {
                dashboard.selected = selected;
                dashboard.error = None;
            }
        }
    }
    app.mark_dirty();
}

#[cfg(test)]
#[path = "global_dashboard_tests.rs"]
pub(crate) mod tests;
