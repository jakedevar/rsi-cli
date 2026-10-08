//! Global manager workspace (#1213, #1231): one full-screen operator console
//! above every project. The left side lists the manager seats (the global
//! seat, then each granted project's PM, with portfolio signals) from one
//! bounded operator-only snapshot (`GetGlobalManagerWorkspace`). The right side
//! shows the selected seat's live conversation and input bar, drawn by the
//! same renderer as the session detail pane, so the operator can read and
//! talk to any manager without leaving the view.
//!
//! The view is an overlay rather than a pane because every tab is bound to a
//! project: a pane would live inside the active project tab, while this view
//! must look and work the same from any tab. Opening a session in its tab
//! (Enter) closes the view and moves to a tab showing that session's project.
//!
//! `n` instantiates a manager in place: pick the seat, a launch from the
//! allowed catalog and the scope, then launch and appoint in one flow
//! (`global_manager_workspace_launch`). The appointment commands
//! (`:manager global appoint|revoke`, `:manager node`, `:manager tree`) still
//! work; the view refreshes after any manager command runs.
//!
//! Seats are `SeatEntry` rows with a depth, so further manager levels (#1230)
//! add rows to `rows()` without changing the console.
//!
//! #1240: the console renders any manager node. `gm` opens the global through
//! the v0 shim (`GetGlobalManagerWorkspace`); Enter on a Portfolio, Project or
//! Area row of the manager tree (and on a child row here) opens that node's
//! console from `GetManagerNodeWorkspace` (`target` is `Some`). A node's
//! children follow its seat: child portfolio nodes with the projects they
//! cover beneath them, then the projects it manages directly, then child
//! areas.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::global_manager::{
    GlobalManagerGrantV1, GlobalManagerWorkspaceV1, GlobalSeatSessionV1, GlobalWorkspaceProjectV1,
    SeatPredecessorV1,
};
use rsi_common::manager_node_workspace::{ManagerNodeChildV1, ManagerNodeWorkspaceV1};
use rsi_common::manager_tier_routing::ManagerNodeRefV1;
use rsi_common::types::SessionStatus;
use uuid::Uuid;

use crate::app::App;
use crate::overlay::global_manager_workspace_launch::{
    FormOutcome, LaunchForm, LaunchRole, handle_form_key, launch_and_appoint,
};
use crate::types::{OverlayState, PopupMode};

/// While the workspace is open, re-read the snapshot this often.
pub const AUTO_REFRESH: Duration = Duration::from_secs(15);

/// The in-view way to fill a seat; shown in the empty, missing and revoked
/// states.
pub const LAUNCH_HINT: &str = "press n to launch and appoint a manager";

/// The command that appoints an existing session as the seat.
pub const APPOINT_HINT: &str = ":manager global appoint [project names...]";

/// Below this inner width the seat list and the conversation take turns
/// (Tab switches) instead of sitting side by side.
pub const SPLIT_MIN_WIDTH: u16 = 96;

/// One seat's health, worst first. Shared by the global seat and each PM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatHealth {
    /// The grant or scope was revoked: the seat holds no authority.
    Revoked,
    /// No seat appointed, or its session no longer exists.
    Missing,
    /// The PM's policy is paused by the operator.
    Paused,
    /// Waiting on the operator (an approval or a pending question).
    Waiting,
    /// Failed or interrupted.
    Stopped,
    /// Starting or running a turn.
    Active,
    /// Between turns; mail and wakes resume it.
    Idle,
}

impl SeatHealth {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Revoked => "REVOKED",
            Self::Missing => "MISSING",
            Self::Paused => "PAUSED",
            Self::Waiting => "WAITING",
            Self::Stopped => "STOPPED",
            Self::Active => "ACTIVE",
            Self::Idle => "IDLE",
        }
    }

    const fn from_status(status: SessionStatus, pending_question: bool) -> Self {
        if pending_question {
            return Self::Waiting;
        }
        match status {
            SessionStatus::WaitingApproval => Self::Waiting,
            SessionStatus::Starting | SessionStatus::Running => Self::Active,
            SessionStatus::Completed => Self::Idle,
            SessionStatus::Failed | SessionStatus::Interrupted => Self::Stopped,
            SessionStatus::Archived | SessionStatus::Deleted => Self::Missing,
            _ => Self::Idle,
        }
    }

    /// What the operator can do about a seat in this state, if anything.
    #[must_use]
    pub const fn action(self) -> Option<&'static str> {
        match self {
            Self::Revoked => Some(
                "Revoked: this seat holds no authority. Press n to launch and appoint a new manager.",
            ),
            Self::Missing => {
                Some("No live manager in this seat. Press n to launch and appoint one.")
            }
            Self::Paused => {
                Some("Paused by the operator's policy; resume it with :manager policy.")
            }
            Self::Waiting => Some("Waiting on the operator: answer its question or approval."),
            Self::Stopped => {
                Some("The seat session stopped. Press i to resume it, or n to replace it.")
            }
            Self::Active | Self::Idle => None,
        }
    }
}

/// The global seat's health under `grant`.
#[must_use]
pub fn seat_health(grant: &GlobalManagerGrantV1, seat: Option<&GlobalSeatSessionV1>) -> SeatHealth {
    if grant.state != "active" {
        return SeatHealth::Revoked;
    }
    seat.map_or(SeatHealth::Missing, |seat| {
        SeatHealth::from_status(seat.status, seat.pending_question)
    })
}

/// One granted project's PM health.
#[must_use]
pub fn pm_health(project: &GlobalWorkspaceProjectV1) -> SeatHealth {
    let overview = &project.overview;
    let policy = overview.policy.as_ref();
    if project.scope_revoked || policy.is_some_and(|policy| policy.revoked) {
        return SeatHealth::Revoked;
    }
    let Some(manager) = &overview.manager else {
        return SeatHealth::Missing;
    };
    if policy.is_some_and(|policy| policy.paused) {
        return SeatHealth::Paused;
    }
    SeatHealth::from_status(manager.status, manager.pending_question)
}

/// A selectable row of the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRow {
    /// The global manager seat.
    Seat,
    /// `projects[index]`.
    Project(usize),
    /// `missing_project_ids[index]`: granted, but the project is gone.
    MissingProject(usize),
    /// #1240: `node.children[index]`: a child portfolio node or area.
    Child(usize),
    /// #1627: an earlier session of a seat (a rotation or succession), read
    /// only. `child` is `None` for the console's own seat, else the child
    /// whose seat it belonged to; `index` is into that seat's `predecessors`.
    Predecessor { child: Option<usize>, index: usize },
}

/// The manager level a seat sits at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatLevel {
    /// The global seat above every project.
    Global,
    /// A project's manager (PM).
    Project,
    /// #1240: a portfolio node of any tier (`SeatEntry::tier` names it).
    Portfolio,
    /// #1240: an area node inside a project.
    Area,
}

impl SeatLevel {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Global => "GLOBAL",
            Self::Project => "PM",
            Self::Portfolio => "NODE",
            Self::Area => "AREA",
        }
    }
}

/// One manager seat at any level: what the list shows and what the
/// conversation pane binds to.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatEntry {
    pub row: WorkspaceRow,
    pub level: SeatLevel,
    /// A portfolio seat's tier label ("pinnacle", "global", ...).
    pub tier: Option<String>,
    /// Indentation level in the seat list (0 = top).
    pub depth: usize,
    pub label: String,
    pub health: SeatHealth,
    /// The live seat session, when one exists.
    pub session_id: Option<Uuid>,
    /// The project the seat governs (PM) or lives in (global seat).
    pub project_id: Option<Uuid>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_fill_pct: Option<f64>,
    pub cost_usd: Option<f64>,
    pub updated_at: Option<DateTime<Utc>>,
    /// Issues open / in progress / operator requests (a sum for the global seat).
    pub issues: Option<(i64, i64, i64)>,
    /// Running, waiting, questions, approvals (a sum for the global seat).
    pub activity: Option<(i64, i64, i64, i64)>,
}

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceFocus {
    /// The seat list.
    #[default]
    Seats,
    /// The conversation transcript (j/k scroll).
    Conversation,
    /// The conversation's input bar (typing).
    Input,
}

#[derive(Debug, Default)]
pub struct GlobalManagerWorkspaceState {
    /// `None` until the first snapshot arrives.
    pub snapshot: Option<GlobalManagerWorkspaceV1>,
    pub selected: usize,
    /// The last refresh failure; the previous snapshot stays on screen.
    pub error: Option<String>,
    /// The last success (an appointment), cleared by the next error.
    pub notice: Option<String>,
    pub loaded_at: Option<DateTime<Utc>>,
    pub last_attempt: Option<Instant>,
    pub focus: WorkspaceFocus,
    /// The instantiate-a-manager form, while open.
    pub launch: Option<LaunchForm>,
    /// #1240: the node this console shows; `None` is the global (`gm`).
    pub target: Option<ManagerNodeRefV1>,
    /// #1240: the node snapshot behind `snapshot` when `target` is set.
    pub node: Option<ManagerNodeWorkspaceV1>,
}

fn sum<I: IntoIterator<Item = i64>>(values: I) -> i64 {
    values.into_iter().sum()
}

impl GlobalManagerWorkspaceState {
    /// Rows in display order: the seat, then each project, then each granted
    /// project that no longer exists. Empty without a grant.
    #[must_use]
    pub fn rows(&self) -> Vec<WorkspaceRow> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        if let Some(node) = &self.node {
            return node_rows(node);
        }
        if snapshot.grant.is_none() {
            return Vec::new();
        }
        std::iter::once(WorkspaceRow::Seat)
            .chain(predecessor_rows(snapshot.seat.as_ref(), None))
            .chain((0..snapshot.projects.len()).map(WorkspaceRow::Project))
            .chain((0..snapshot.missing_project_ids.len()).map(WorkspaceRow::MissingProject))
            .collect()
    }

    #[must_use]
    pub fn selected_row(&self) -> Option<WorkspaceRow> {
        self.rows().get(self.selected).copied()
    }

    /// Whether there is a seat list to show (a grant, or any node snapshot).
    #[must_use]
    pub fn has_seats(&self) -> bool {
        self.node.is_some() || self.grant().is_some()
    }

    #[must_use]
    pub fn child(&self, index: usize) -> Option<&ManagerNodeChildV1> {
        self.node.as_ref().and_then(|node| node.children.get(index))
    }

    #[must_use]
    pub fn grant(&self) -> Option<&GlobalManagerGrantV1> {
        self.snapshot.as_ref().and_then(|s| s.grant.as_ref())
    }

    #[must_use]
    pub fn project(&self, index: usize) -> Option<&GlobalWorkspaceProjectV1> {
        self.snapshot.as_ref().and_then(|s| s.projects.get(index))
    }

    /// The global seat's session id, when the seat session still exists.
    #[must_use]
    pub fn seat_session_id(&self) -> Option<Uuid> {
        self.snapshot
            .as_ref()
            .and_then(|s| s.seat.as_ref())
            .map(|seat| seat.session_id)
    }

    /// The seat behind `row`.
    #[must_use]
    pub fn seat(&self, row: WorkspaceRow) -> Option<SeatEntry> {
        let snapshot = self.snapshot.as_ref()?;
        if let Some(node) = &self.node {
            match row {
                WorkspaceRow::Seat => return node_seat(node),
                WorkspaceRow::Child(index) => return child_seat(node.children.get(index)?, index),
                WorkspaceRow::Project(index) => {
                    let mut entry = self.project_seat(row, snapshot.projects.get(index)?);
                    entry.depth = node_project_depth(node, entry.project_id);
                    return Some(entry);
                }
                WorkspaceRow::Predecessor { child, index } => {
                    return predecessor_seat(self, node, child, index);
                }
                WorkspaceRow::MissingProject(_) => {}
            }
        }
        match row {
            WorkspaceRow::Child(_) => None,
            WorkspaceRow::Predecessor { child: None, index } => {
                let owner = self.seat(WorkspaceRow::Seat)?;
                let prior = snapshot.seat.as_ref()?.predecessors.get(index)?;
                Some(predecessor_entry(&owner, row, prior))
            }
            WorkspaceRow::Predecessor { .. } => None,
            WorkspaceRow::Seat => {
                let grant = snapshot.grant.as_ref()?;
                let seat = snapshot.seat.as_ref();
                let overviews = || snapshot.projects.iter().map(|p| &p.overview);
                Some(SeatEntry {
                    row,
                    level: SeatLevel::Global,
                    tier: None,
                    depth: 0,
                    label: "global manager".into(),
                    health: seat_health(grant, seat),
                    session_id: seat.map(|s| s.session_id),
                    project_id: seat.and_then(|s| s.project_id),
                    model: seat.and_then(|s| s.model.clone()),
                    effort: None,
                    context_fill_pct: seat.and_then(|s| s.context_fill_pct),
                    cost_usd: seat.and_then(|s| s.cost_usd),
                    updated_at: seat.map(|s| s.updated_at),
                    issues: Some((
                        sum(overviews().map(|o| o.issues.open)),
                        sum(overviews().map(|o| o.issues.in_progress)),
                        sum(overviews().map(|o| o.issues.open_operator_requests)),
                    )),
                    activity: Some((
                        sum(overviews().map(|o| o.running_sessions)),
                        sum(overviews().map(|o| o.waiting_approval_sessions)),
                        sum(overviews().map(|o| o.pending_questions)),
                        sum(overviews().map(|o| o.pending_approvals)),
                    )),
                })
            }
            WorkspaceRow::Project(index) => {
                Some(self.project_seat(row, snapshot.projects.get(index)?))
            }
            WorkspaceRow::MissingProject(index) => {
                let id = *snapshot.missing_project_ids.get(index)?;
                Some(SeatEntry {
                    row,
                    level: SeatLevel::Project,
                    tier: None,
                    depth: 1,
                    label: format!("deleted {}", short(id)),
                    health: SeatHealth::Missing,
                    session_id: None,
                    project_id: None,
                    model: None,
                    effort: None,
                    context_fill_pct: None,
                    cost_usd: None,
                    updated_at: None,
                    issues: None,
                    activity: None,
                })
            }
        }
    }

    /// A project's PM seat row.
    fn project_seat(&self, row: WorkspaceRow, project: &GlobalWorkspaceProjectV1) -> SeatEntry {
        let mut entry = pm_entry(project);
        entry.row = row;
        entry
    }

    /// Every seat in display order.
    #[must_use]
    pub fn seats(&self) -> Vec<SeatEntry> {
        self.rows()
            .into_iter()
            .filter_map(|row| self.seat(row))
            .collect()
    }

    #[must_use]
    pub fn selected_seat(&self) -> Option<SeatEntry> {
        self.selected_row().and_then(|row| self.seat(row))
    }

    /// The selected project's live effective ceilings, supplied by the daemon.
    #[must_use]
    pub fn selected_cap_line(&self) -> Option<String> {
        let seat = self.selected_seat()?;
        if seat.level != SeatLevel::Project {
            return None;
        }
        let project_id = seat.project_id?;
        let caps = self
            .snapshot
            .as_ref()?
            .projects
            .iter()
            .find(|project| project.overview.project_id == project_id)?
            .overview
            .policy
            .as_ref()?
            .effective_caps
            .as_ref()?;
        let providers = caps
            .provider_limits
            .iter()
            .map(|limit| format!("{:?} {}", limit.provider, limit.max_active))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Effective caps · active {} · created sessions {} · containers {} · spend {} · provider ceilings [{}]",
            caps.max_active_sessions,
            caps.max_created_sessions,
            caps.max_created_containers,
            caps.max_spend_usd
                .map_or_else(|| "uncapped".into(), |spend| format!("${spend}")),
            providers,
        ))
    }

    /// The session the conversation pane shows: the selected seat's.
    #[must_use]
    pub fn conversation_session_id(&self) -> Option<Uuid> {
        self.selected_seat().and_then(|seat| seat.session_id)
    }

    /// Select the first row matching `pred`; false when none does.
    pub fn select_where(&mut self, pred: impl Fn(&SeatEntry) -> bool) -> bool {
        match self.seats().iter().position(pred) {
            Some(index) => {
                self.selected = index;
                true
            }
            None => false,
        }
    }

    /// Install a fresh snapshot, keeping the selection on the same row kind
    /// and project where it still exists.
    /// #1240: install a node snapshot (`target` must name it).
    pub fn install_node(&mut self, node: ManagerNodeWorkspaceV1, now: DateTime<Utc>) {
        let keep_child = match self.selected_row() {
            Some(WorkspaceRow::Child(index)) => self.child(index).map(|child| child.node),
            _ => None,
        };
        self.node = Some(node.clone());
        self.install(node.into_global(), now);
        if let Some(keep) = keep_child {
            let rows = self.rows();
            if let Some(position) = rows.iter().position(|row| {
                matches!(row, WorkspaceRow::Child(index)
                    if self.child(*index).is_some_and(|child| child.node == keep))
            }) {
                self.selected = position;
            }
        }
    }

    pub fn install(&mut self, snapshot: GlobalManagerWorkspaceV1, now: DateTime<Utc>) {
        let keep_project = match self.selected_row() {
            Some(WorkspaceRow::Project(index)) => {
                self.project(index).map(|p| p.overview.project_id)
            }
            _ => None,
        };
        self.snapshot = Some(snapshot);
        self.loaded_at = Some(now);
        self.error = None;
        let rows = self.rows();
        let target = keep_project.and_then(|id| {
            rows.iter().position(|row| {
                matches!(row, WorkspaceRow::Project(index)
                    if self.project(*index).is_some_and(|p| p.overview.project_id == id))
            })
        });
        self.selected = target
            .unwrap_or(self.selected)
            .min(rows.len().saturating_sub(1));
    }

    /// One header line describing the grant (or its absence).
    #[must_use]
    pub fn grant_line(&self, projects: &[rsi_common::types::Project]) -> String {
        let Some(grant) = self.grant() else {
            return "No global manager is appointed.".into();
        };
        let names: Vec<String> = grant
            .project_ids
            .iter()
            .map(|id| {
                projects
                    .iter()
                    .find(|project| project.id == *id)
                    .map_or_else(|| short(*id), |project| project.name.clone())
            })
            .collect();
        let launches: Vec<String> = grant
            .allowed_launches
            .iter()
            .map(|launch| {
                format!(
                    "{:?}/{}/{}",
                    launch.provider,
                    launch.model,
                    launch.effort.as_deref().unwrap_or("default")
                )
            })
            .collect();
        format!(
            "Grant v{} {} · PM policy {:?} · launches [{}] · {} project{} [{}]",
            grant.grant_version,
            grant.state,
            grant.project_policy.mode,
            launches.join(", "),
            grant.project_ids.len(),
            if grant.project_ids.len() == 1 {
                ""
            } else {
                "s"
            },
            names.join(", "),
        )
    }
}

/// How a node is named in the console: its tier, project or area.
fn node_ref_text(node: ManagerNodeRefV1, projects: &[rsi_common::types::Project]) -> String {
    match node {
        ManagerNodeRefV1::Portfolio { node_id } => format!("portfolio node {}", short(node_id)),
        ManagerNodeRefV1::Project { project_id } => projects
            .iter()
            .find(|project| project.id == project_id)
            .map_or_else(
                || format!("project {}", short(project_id)),
                |project| format!("project {}", project.name),
            ),
        ManagerNodeRefV1::Area { node_id } => format!("area {}", short(node_id)),
    }
}

/// `tokens/min $/h` of one fleet window.
fn rate(usage: &rsi_common::fleet::FleetUsage, seconds: i64) -> String {
    let tpm = usage.tokens_per_minute(seconds);
    let tokens = if tpm >= 1000.0 {
        format!("{:.1}k", tpm / 1000.0)
    } else {
        format!("{tpm:.0}")
    };
    format!("{tokens} tok/min ${:.2}/h", usage.cost_per_hour(seconds))
}

impl GlobalManagerWorkspaceState {
    /// #1240: the console title (`None` for the global's `gm` console).
    #[must_use]
    pub fn node_title(&self) -> Option<String> {
        self.target?;
        let Some(node) = self.node.as_ref() else {
            return Some("Manager console".into());
        };
        Some(match node.node {
            ManagerNodeRefV1::Portfolio { .. } => format!("{} manager console", node.label),
            ManagerNodeRefV1::Project { .. } => format!("Project manager console · {}", node.label),
            ManagerNodeRefV1::Area { node_id } => {
                format!("Area manager console · {}", short(node_id))
            }
        })
    }

    /// #1240: the node console's header: where the node sits, its grant,
    /// its fleet rollup and the escalations waiting on it.
    #[must_use]
    pub fn node_lines(&self, projects: &[rsi_common::types::Project]) -> Vec<String> {
        let Some(node) = &self.node else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        let above = node.parent.map_or_else(
            || "the operator".to_string(),
            |p| node_ref_text(p, projects),
        );
        let below = node.children.len();
        lines.push(format!(
            "Reports to {above} · {below} child{} · {} project{} covered",
            if below == 1 { "" } else { "ren" },
            node.projects.len() + node.missing_project_ids.len(),
            if node.projects.len() + node.missing_project_ids.len() == 1 {
                ""
            } else {
                "s"
            },
        ));
        match node.node {
            ManagerNodeRefV1::Portfolio { .. } => {
                let mut line = self.grant_line(projects);
                if let Some(grantor) = &node.grantor {
                    line.push_str(&format!(" · granted by {grantor}"));
                }
                lines.push(line);
            }
            ManagerNodeRefV1::Project { .. } => {
                if let Some(project) = node.projects.first() {
                    let scope = project.overview.manager.as_ref().map_or_else(
                        || "no PM seat".into(),
                        |m| format!("PM scope v{}", m.scope_version),
                    );
                    let policy = project.overview.policy.as_ref().map_or_else(
                        || "no policy".into(),
                        |policy| {
                            format!(
                                "policy v{} {:?}{}{}",
                                policy.policy_version,
                                policy.mode,
                                if policy.paused { " paused" } else { "" },
                                if policy.revoked { " revoked" } else { "" },
                            )
                        },
                    );
                    lines.push(format!("{scope} · {policy}"));
                }
            }
            ManagerNodeRefV1::Area { .. } => {
                if let Some(area) = &node.area {
                    let selector = match &area.selector {
                        Some(rsi_common::manager_nodes::ManagerNodeSelectorV1::Selected {
                            group_ids,
                            epic_ids,
                        }) => format!(
                            "{} group{}, {} epic{}",
                            group_ids.len(),
                            if group_ids.len() == 1 { "" } else { "s" },
                            epic_ids.len(),
                            if epic_ids.len() == 1 { "" } else { "s" },
                        ),
                        _ => "whole project".into(),
                    };
                    lines.push(format!(
                        "Area grant v{} {} · {selector} in {}",
                        area.grant_version,
                        if area.active { "active" } else { "revoked" },
                        node_ref_text(
                            ManagerNodeRefV1::Project {
                                project_id: area.project_id
                            },
                            projects
                        ),
                    ));
                }
            }
        }
        let fleet = &node.fleet;
        lines.push(format!(
            "Fleet {} active · 5m {} · 1h {} · 24h {} invocations{}",
            fleet.active,
            rate(&fleet.totals[0], rsi_common::fleet::FLEET_WINDOWS[0]),
            rate(&fleet.totals[1], rsi_common::fleet::FLEET_WINDOWS[1]),
            fleet.totals[2].invocations,
            if fleet.agents_truncated || fleet.usage_truncated {
                " (partial)"
            } else {
                ""
            },
        ));
        if let Some(first) = node.escalations.first() {
            lines.push(format!(
                "{} escalation{} waiting on this node{}: {}",
                node.escalations.len(),
                if node.escalations.len() == 1 { "" } else { "s" },
                if node.escalations_truncated { "+" } else { "" },
                first.reason
            ));
        }
        lines
    }
}

/// The predecessor rows listed under a seat.
fn predecessor_rows(
    seat: Option<&GlobalSeatSessionV1>,
    child: Option<usize>,
) -> impl Iterator<Item = WorkspaceRow> {
    (0..seat.map_or(0, |seat| seat.predecessors.len()))
        .map(move |index| WorkspaceRow::Predecessor { child, index })
}

/// #1627: a predecessor row: the owner's level one step deeper, bound to the
/// earlier session so the conversation pane shows it read only.
fn predecessor_entry(owner: &SeatEntry, row: WorkspaceRow, prior: &SeatPredecessorV1) -> SeatEntry {
    let health = match SeatHealth::from_status(prior.status, false) {
        SeatHealth::Missing => SeatHealth::Idle,
        health => health,
    };
    SeatEntry {
        row,
        level: owner.level,
        tier: owner.tier.clone(),
        depth: owner.depth + 1,
        label: format!("↳ prior {}", short(prior.session_id)),
        health,
        session_id: Some(prior.session_id),
        project_id: prior.project_id,
        model: prior.model.clone(),
        effort: None,
        context_fill_pct: prior.context_fill_pct,
        cost_usd: prior.cost_usd,
        updated_at: Some(prior.updated_at),
        issues: None,
        activity: None,
    }
}

fn predecessor_seat(
    state: &GlobalManagerWorkspaceState,
    node: &ManagerNodeWorkspaceV1,
    child: Option<usize>,
    index: usize,
) -> Option<SeatEntry> {
    let row = WorkspaceRow::Predecessor { child, index };
    let (owner, seat) = match child {
        None => (state.seat(WorkspaceRow::Seat)?, node.seat.as_ref()?),
        Some(child) => (
            state.seat(WorkspaceRow::Child(child))?,
            node.children.get(child)?.seat.as_ref()?,
        ),
    };
    Some(predecessor_entry(
        &owner,
        row,
        seat.predecessors.get(index)?,
    ))
}

fn short(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

/// A covered project's PM seat row (depth 1).
fn pm_entry(project: &GlobalWorkspaceProjectV1) -> SeatEntry {
    let overview = &project.overview;
    let manager = overview.manager.as_ref();
    SeatEntry {
        row: WorkspaceRow::Seat,
        level: SeatLevel::Project,
        tier: None,
        depth: 1,
        label: overview.name.clone(),
        health: pm_health(project),
        session_id: manager.map(|m| m.session_id),
        project_id: Some(overview.project_id),
        model: manager.and_then(|m| m.model.clone()),
        effort: manager.and_then(|m| m.effort.clone()),
        context_fill_pct: manager.and_then(|m| m.context_fill_pct),
        cost_usd: manager.and_then(|m| m.cost_usd),
        updated_at: manager.map(|m| m.updated_at),
        issues: Some((
            overview.issues.open,
            overview.issues.in_progress,
            overview.issues.open_operator_requests,
        )),
        activity: Some((
            overview.running_sessions,
            overview.waiting_approval_sessions,
            overview.pending_questions,
            overview.pending_approvals,
        )),
    }
}

/// The child portfolio node covering `project`, if one is listed.
fn covering_child(node: &ManagerNodeWorkspaceV1, project: Uuid) -> Option<usize> {
    node.children.iter().position(|child| {
        matches!(child.node, ManagerNodeRefV1::Portfolio { .. })
            && child.project_ids.contains(&project)
    })
}

/// #1240: a node's rows: its seat; each child portfolio node with the
/// projects it covers beneath it; the projects the node manages directly;
/// child areas; granted projects that are gone.
fn node_rows(node: &ManagerNodeWorkspaceV1) -> Vec<WorkspaceRow> {
    let mut rows = vec![WorkspaceRow::Seat];
    rows.extend(predecessor_rows(node.seat.as_ref(), None));
    if !matches!(node.node, ManagerNodeRefV1::Portfolio { .. }) {
        // A project or area node: its own project is the seat row's.
        for index in 0..node.children.len() {
            push_child(&mut rows, node, index);
        }
        return rows;
    }
    for (index, child) in node.children.iter().enumerate() {
        if !matches!(child.node, ManagerNodeRefV1::Portfolio { .. }) {
            continue;
        }
        push_child(&mut rows, node, index);
        rows.extend(
            node.projects
                .iter()
                .enumerate()
                .filter(|(_, project)| child.project_ids.contains(&project.overview.project_id))
                .map(|(position, _)| WorkspaceRow::Project(position)),
        );
    }
    rows.extend(
        node.projects
            .iter()
            .enumerate()
            .filter(|(_, project)| covering_child(node, project.overview.project_id).is_none())
            .map(|(position, _)| WorkspaceRow::Project(position)),
    );
    for (index, child) in node.children.iter().enumerate() {
        if matches!(child.node, ManagerNodeRefV1::Area { .. }) {
            push_child(&mut rows, node, index);
        }
    }
    rows.extend((0..node.missing_project_ids.len()).map(WorkspaceRow::MissingProject));
    rows
}

/// A child's row followed by its seat's predecessors.
fn push_child(rows: &mut Vec<WorkspaceRow>, node: &ManagerNodeWorkspaceV1, index: usize) {
    rows.push(WorkspaceRow::Child(index));
    rows.extend(predecessor_rows(
        node.children[index].seat.as_ref(),
        Some(index),
    ));
}

/// A project row sits under the child node covering it (depth 2), else
/// directly under the node (depth 1).
fn node_project_depth(node: &ManagerNodeWorkspaceV1, project: Option<Uuid>) -> usize {
    match project {
        Some(id) if covering_child(node, id).is_some() => 2,
        _ => 1,
    }
}

/// Health of a seat that has no PM policy: revoked, missing, else its
/// session's status.
fn node_health(state: &str, seat: Option<&GlobalSeatSessionV1>) -> SeatHealth {
    if state == "revoked" {
        return SeatHealth::Revoked;
    }
    seat.map_or(SeatHealth::Missing, |seat| {
        SeatHealth::from_status(seat.status, seat.pending_question)
    })
}

fn seat_fields(entry: &mut SeatEntry, seat: Option<&GlobalSeatSessionV1>) {
    entry.session_id = seat.map(|s| s.session_id);
    entry.project_id = seat.and_then(|s| s.project_id);
    entry.model = seat.and_then(|s| s.model.clone());
    entry.context_fill_pct = seat.and_then(|s| s.context_fill_pct);
    entry.cost_usd = seat.and_then(|s| s.cost_usd);
    entry.updated_at = seat.map(|s| s.updated_at);
}

/// #1240: the node's own seat row.
fn node_seat(node: &ManagerNodeWorkspaceV1) -> Option<SeatEntry> {
    match node.node {
        ManagerNodeRefV1::Project { .. } => {
            let mut entry = node.projects.first().map(pm_entry)?;
            entry.depth = 0;
            Some(entry)
        }
        ManagerNodeRefV1::Portfolio { .. } => {
            let overviews = || node.projects.iter().map(|p| &p.overview);
            let mut entry = SeatEntry {
                row: WorkspaceRow::Seat,
                level: SeatLevel::Portfolio,
                tier: Some(node.label.clone()),
                depth: 0,
                label: format!("{} manager", node.label),
                health: match &node.grant {
                    Some(grant) => seat_health(grant, node.seat.as_ref()),
                    None => node_health(&node.state, node.seat.as_ref()),
                },
                session_id: None,
                project_id: None,
                model: None,
                effort: None,
                context_fill_pct: None,
                cost_usd: None,
                updated_at: None,
                issues: Some((
                    sum(overviews().map(|o| o.issues.open)),
                    sum(overviews().map(|o| o.issues.in_progress)),
                    sum(overviews().map(|o| o.issues.open_operator_requests)),
                )),
                activity: Some((
                    sum(overviews().map(|o| o.running_sessions)),
                    sum(overviews().map(|o| o.waiting_approval_sessions)),
                    sum(overviews().map(|o| o.pending_questions)),
                    sum(overviews().map(|o| o.pending_approvals)),
                )),
            };
            seat_fields(&mut entry, node.seat.as_ref());
            Some(entry)
        }
        ManagerNodeRefV1::Area { node_id } => {
            let mut entry = SeatEntry {
                row: WorkspaceRow::Seat,
                level: SeatLevel::Area,
                tier: None,
                depth: 0,
                label: format!("area {}", short(node_id)),
                health: node_health(&node.state, node.seat.as_ref()),
                session_id: None,
                project_id: None,
                model: None,
                effort: None,
                context_fill_pct: None,
                cost_usd: None,
                updated_at: None,
                issues: None,
                activity: None,
            };
            seat_fields(&mut entry, node.seat.as_ref());
            Some(entry)
        }
    }
}

/// #1240: a child node's digest row.
fn child_seat(child: &ManagerNodeChildV1, index: usize) -> Option<SeatEntry> {
    let (level, tier, label) = match child.node {
        ManagerNodeRefV1::Portfolio { node_id } => (
            SeatLevel::Portfolio,
            Some(child.label.clone()),
            format!("{} {}", child.label, short(node_id)),
        ),
        ManagerNodeRefV1::Area { node_id } => {
            (SeatLevel::Area, None, format!("area {}", short(node_id)))
        }
        ManagerNodeRefV1::Project { .. } => (SeatLevel::Project, None, child.label.clone()),
    };
    let mut entry = SeatEntry {
        row: WorkspaceRow::Child(index),
        level,
        tier,
        depth: 1,
        label,
        health: if child.state == "vacant" {
            SeatHealth::Missing
        } else {
            node_health(&child.state, child.seat.as_ref())
        },
        session_id: None,
        project_id: None,
        model: None,
        effort: None,
        context_fill_pct: None,
        cost_usd: None,
        updated_at: None,
        issues: child.counts.map(|counts| {
            (
                counts.issues.open,
                counts.issues.in_progress,
                counts.issues.open_operator_requests,
            )
        }),
        activity: child.counts.map(|counts| {
            (
                counts.running_sessions,
                counts.waiting_approval_sessions,
                counts.pending_questions,
                counts.pending_approvals,
            )
        }),
    };
    seat_fields(&mut entry, child.seat.as_ref());
    Some(entry)
}

fn pct(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |pct| format!("{pct:.0}%"))
}

fn cost(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |usd| format!("${usd:.2}"))
}

fn fit(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

/// Optional seat-list columns and the list width each needs; the base
/// columns (seat, health, context, issues) fit in 46.
const MODEL_WIDTH: usize = 65;
const ACTIVITY_WIDTH: usize = 83;
const UPDATED_WIDTH: usize = 96;

/// The seat list's column header for a list `width` columns wide.
#[must_use]
pub fn seat_header(width: usize) -> String {
    let mut out = format!(
        "  {} {:<8} {:>4}  {:<8}",
        fit("SEAT", 20),
        "HEALTH",
        "CTX",
        "ISSUES"
    );
    if width >= MODEL_WIDTH {
        out.push_str(&format!(" {:<18}", "MODEL"));
    }
    if width >= ACTIVITY_WIDTH {
        out.push_str(&format!(
            " {:>4} {:>4} {:>2} {:>4}",
            "RUN", "WAIT", "Q", "APPR"
        ));
    }
    if width >= UPDATED_WIDTH {
        out.push_str("  UPDATED");
    }
    out.trim_end().to_string()
}

/// One seat's list row (without the selection marker), aligned under
/// `seat_header(width)`. Columns appear as the width allows.
#[must_use]
pub fn seat_row_text(seat: &SeatEntry, width: usize) -> String {
    let indent = "  ".repeat(seat.depth);
    let name = fit(&format!("{indent}{}", seat.label), 20);
    let issues = seat.issues.map_or_else(
        || "-".into(),
        |(open, prog, req)| format!("{open}/{prog}/{req}"),
    );
    let mut out = format!(
        "{name} {:<8} {:>4}  {:<8}",
        seat.health.label(),
        pct(seat.context_fill_pct),
        issues
    );
    if width >= MODEL_WIDTH {
        out.push_str(&format!(
            " {}",
            fit(seat.model.as_deref().unwrap_or("-"), 18)
        ));
    }
    if width >= ACTIVITY_WIDTH {
        match seat.activity {
            Some((run, wait, q, appr)) => {
                out.push_str(&format!(" {run:>4} {wait:>4} {q:>2} {appr:>4}"));
            }
            None => out.push_str(&format!(" {:>4} {:>4} {:>2} {:>4}", "-", "-", "-", "-")),
        }
    }
    if width >= UPDATED_WIDTH {
        out.push_str(&format!(
            "  {}",
            seat.updated_at
                .map_or_else(|| "-".into(), |at| at.format("%m-%d %H:%M").to_string())
        ));
    }
    out.trim_end().to_string()
}

/// The selected seat's description, shown above its conversation: who it
/// is, then its portfolio signals.
#[must_use]
pub fn seat_summary(seat: &SeatEntry) -> Vec<String> {
    let mut identity = vec![
        match seat.level {
            SeatLevel::Global => "GLOBAL SEAT".to_string(),
            SeatLevel::Project => format!("PM · {}", seat.label),
            SeatLevel::Portfolio => {
                let tier = seat.tier.as_deref().unwrap_or("portfolio").to_uppercase();
                if seat.depth == 0 {
                    format!("{tier} SEAT")
                } else {
                    format!("{tier} SEAT · {}", seat.label)
                }
            }
            SeatLevel::Area => format!("AREA · {}", seat.label),
        },
        seat.health.label().to_string(),
    ];
    if let Some(model) = &seat.model {
        identity.push(match &seat.effort {
            Some(effort) => format!("{model}/{effort}"),
            None => model.clone(),
        });
    }
    if let Some(id) = seat.session_id {
        identity.push(format!("session {}", short(id)));
    }
    identity.push(format!("ctx {}", pct(seat.context_fill_pct)));
    identity.push(cost(seat.cost_usd));
    if let Some(at) = seat.updated_at {
        identity.push(format!("updated {}", at.format("%m-%d %H:%M")));
    }
    let mut portfolio = Vec::new();
    if let Some((open, prog, req)) = seat.issues {
        portfolio.push(format!(
            "Issues {open} open, {prog} in progress, {req} operator request{}",
            if req == 1 { "" } else { "s" }
        ));
    }
    if let Some((run, wait, q, appr)) = seat.activity {
        portfolio.push(format!(
            "{run} running, {wait} waiting, {q} question{}, {appr} approval{}",
            if q == 1 { "" } else { "s" },
            if appr == 1 { "" } else { "s" }
        ));
    }
    let mut lines = vec![identity.join(" · ")];
    if !portfolio.is_empty() {
        lines.push(portfolio.join(" · "));
    }
    lines
}

/// The missing-seat line for a granted project that no longer exists.
#[must_use]
pub fn missing_project_text(id: Uuid) -> String {
    format!(
        "{} {:<8} granted project no longer exists; re-appoint the grant without it ({LAUNCH_HINT})",
        short(id),
        SeatHealth::Missing.label()
    )
}

fn state(app: &mut App) -> Option<&mut GlobalManagerWorkspaceState> {
    match &mut app.overlay {
        OverlayState::GlobalManagerWorkspace(state) => Some(state),
        _ => None,
    }
}

#[must_use]
pub fn is_open(app: &App) -> bool {
    matches!(app.overlay, OverlayState::GlobalManagerWorkspace(..))
}

/// The session the open workspace's conversation pane shows.
#[must_use]
pub fn open_conversation_session(app: &App) -> Option<Uuid> {
    match &app.overlay {
        OverlayState::GlobalManagerWorkspace(state) if state.launch.is_none() => {
            state.conversation_session_id()
        }
        _ => None,
    }
}

/// The workspace owns typed text (its input bar or the launch form), so the
/// overlay leader must not take Space.
#[must_use]
pub fn owns_text_entry(state: &GlobalManagerWorkspaceState) -> bool {
    state.launch.is_some() || state.focus == WorkspaceFocus::Input
}

/// Open the workspace above whatever overlay or pane is showing, or refresh
/// it when it is already open (`gm` twice focuses and reloads).
pub async fn open(app: &mut App) {
    if !is_open(app) {
        app.overlay = OverlayState::GlobalManagerWorkspace(Box::default());
    }
    refresh(app).await;
    app.mark_dirty();
}

/// #1240: open the console on `node` (from the manager tree, or a child
/// row here). It replaces whatever overlay is showing.
pub async fn open_node(app: &mut App, node: ManagerNodeRefV1) {
    app.overlay = OverlayState::GlobalManagerWorkspace(Box::new(GlobalManagerWorkspaceState {
        target: Some(node),
        ..GlobalManagerWorkspaceState::default()
    }));
    refresh(app).await;
    app.mark_dirty();
}

/// Re-read the snapshot. A failure keeps the previous snapshot on screen and
/// marks it stale with an actionable error.
pub async fn refresh(app: &mut App) {
    let target = if let Some(workspace) = state(app) {
        workspace.last_attempt = Some(Instant::now());
        workspace.target
    } else {
        return;
    };
    let result = match target {
        None => app
            .client
            .get_global_manager_workspace()
            .await
            .map(Snapshot::Global),
        Some(node) => app
            .client
            .get_manager_node_workspace(node)
            .await
            .map(|node| Snapshot::Node(Box::new(node))),
    };
    let Some(workspace) = state(app) else { return };
    match result {
        Ok(snapshot) => {
            match snapshot {
                Snapshot::Global(snapshot) => workspace.install(snapshot, Utc::now()),
                Snapshot::Node(node) => workspace.install_node(*node, Utc::now()),
            }
            ensure_conversation(app).await;
        }
        Err(error) => {
            let stale = if workspace.snapshot.is_some() {
                " (showing the last snapshot)"
            } else {
                ""
            };
            workspace.error = Some(format!("Refresh failed: {error}{stale}; press r to retry"));
        }
    }
    app.mark_dirty();
}

enum Snapshot {
    Global(GlobalManagerWorkspaceV1),
    Node(Box<ManagerNodeWorkspaceV1>),
}

/// Refresh after a manager or grant command, when the workspace is open.
pub async fn refresh_if_open(app: &mut App) {
    if is_open(app) {
        refresh(app).await;
    }
}

/// Event-loop tick: re-read the snapshot every `AUTO_REFRESH` while open.
pub async fn tick(app: &mut App) {
    let due = match &app.overlay {
        OverlayState::GlobalManagerWorkspace(state) => state
            .last_attempt
            .is_none_or(|at| at.elapsed() >= AUTO_REFRESH),
        _ => false,
    };
    if due {
        refresh(app).await;
    }
}

/// Make sure the selected seat's session is cached and its conversation is
/// loading. Live events then arrive on the shared notification stream, which
/// appends to every cached session.
pub async fn ensure_conversation(app: &mut App) {
    let Some(session_id) = state(app).and_then(|w| w.conversation_session_id()) else {
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
                if let Some(workspace) = state(app) {
                    workspace.error = Some(format!(
                        "Seat session {} unavailable: {error}; press r to refresh or n to replace it",
                        short(session_id)
                    ));
                }
                return;
            }
        }
    }
    app.trigger_focus_fetch_if_needed(session_id);
    app.mark_dirty();
}

/// Make the active tab show `project_id`'s sessions: stay on an "all" tab,
/// else switch to the project's tab (opening one when none exists). A
/// session outside every project moves to the "all" tab.
pub fn focus_project_tab(app: &mut App, project_id: Option<Uuid>) {
    let Some(active) = app.active_project_id() else {
        return;
    };
    match project_id {
        Some(project_id) if project_id == active => {}
        Some(project_id) => {
            if let Some(index) = app
                .tabs
                .iter()
                .position(|tab| tab.project_id == Some(project_id))
            {
                app.active_tab = index;
                app.sync_project_filter();
            } else {
                app.open_project_workspace(project_id);
            }
        }
        None => app.open_all_projects_workspace(),
    }
}

/// Close the workspace and open `session_id` in the focused pane of a tab
/// showing its project.
pub async fn jump_to_session(app: &mut App, session_id: Uuid) -> bool {
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
                if let Some(workspace) = state(app) {
                    workspace.error = Some(format!(
                        "Session {} unavailable: {error}; press r to refresh",
                        short(session_id)
                    ));
                }
                return false;
            }
        }
    }
    let project_id = app
        .sessions
        .get(&session_id)
        .and_then(|state| state.session.project_id);
    app.overlay = OverlayState::None;
    focus_project_tab(app, project_id);
    app.open_session_in_current_pane(session_id);
    true
}

/// Close the workspace on `project_id`'s session list.
pub fn jump_to_project(app: &mut App, project_id: Uuid) -> bool {
    if !app.projects.iter().any(|project| project.id == project_id) {
        if let Some(workspace) = state(app) {
            workspace.error = Some("That project no longer exists; press r to refresh".into());
        }
        return false;
    }
    app.overlay = OverlayState::None;
    focus_project_tab(app, Some(project_id));
    if app.active_project_id() == Some(project_id) {
        app.back_to_list();
    }
    true
}

/// Type to the selected seat in place.
fn start_typing(app: &mut App) {
    let Some(workspace) = state(app) else { return };
    let Some(seat) = workspace.selected_seat() else {
        workspace.error = Some(format!("No manager seat is selected; {LAUNCH_HINT}"));
        return;
    };
    if matches!(seat.row, WorkspaceRow::Predecessor { .. }) {
        workspace.error = Some(
            "An earlier seat session is read-only history; press t to talk to the current seat"
                .into(),
        );
        return;
    }
    let Some(session_id) = seat.session_id else {
        workspace.error = Some(format!(
            "{} has no live session to talk to; {LAUNCH_HINT}",
            seat.label
        ));
        return;
    };
    let Some(status) = app.sessions.get(&session_id).map(|s| s.session.status) else {
        if let Some(workspace) = state(app) {
            workspace.error = Some(format!(
                "Session {} is still loading; press r to refresh",
                short(session_id)
            ));
        }
        return;
    };
    if matches!(status, SessionStatus::Archived | SessionStatus::Deleted) {
        if let Some(workspace) = state(app) {
            workspace.error = Some(format!(
                "Session {} is {status:?} and read-only; {LAUNCH_HINT}",
                short(session_id)
            ));
        }
        return;
    }
    crate::input_bar::enter_insert_mode(
        app,
        session_id,
        crate::modalkit_types::InsertStyle::Insert,
    );
    if let Some(workspace) = state(app) {
        workspace.focus = WorkspaceFocus::Input;
        workspace.error = None;
    }
}

/// Select the console's own seat (the global, or the node shown) and type
/// to it (`t`).
async fn talk(app: &mut App) {
    let Some(workspace) = state(app) else { return };
    if !workspace.select_where(|seat| seat.row == WorkspaceRow::Seat) {
        workspace.error = Some(format!("No global manager is appointed; {LAUNCH_HINT}"));
        return;
    }
    ensure_conversation(app).await;
    start_typing(app);
}

/// #1627 R3: the selected seat row's session, or a refusal on the console.
fn selected_seat_session(app: &mut App, action: &str) -> Option<(SeatEntry, Uuid)> {
    let workspace = state(app)?;
    let Some(seat) = workspace.selected_seat() else {
        workspace.error = Some(format!("Select a seat row to {action}"));
        return None;
    };
    match seat.session_id {
        Some(id) => Some((seat, id)),
        None => {
            workspace.error = Some(format!(
                "{} has no session to {action}; {LAUNCH_HINT}",
                seat.label
            ));
            None
        }
    }
}

/// `s`: one line naming the selected seat session's lifecycle status, the
/// seat health and where the session lives. Read only.
fn seat_status(app: &mut App) {
    let Some((seat, id)) = selected_seat_session(app, "show") else {
        return;
    };
    let status = app
        .sessions
        .get(&id)
        .map(|s| format!("{:?}", s.session.status));
    let Some(workspace) = state(app) else { return };
    let history = if matches!(seat.row, WorkspaceRow::Predecessor { .. }) {
        " (earlier session, read only)"
    } else {
        ""
    };
    let status = status.unwrap_or_else(|| "not loaded".into());
    workspace.error = None;
    workspace.notice = Some(format!(
        "{} {}{history}: status {status}, seat {}",
        seat.label,
        short(id),
        seat.health.label()
    ));
}

/// `x` (soft) / `X` (now, press twice): halt the selected seat's session
/// through the same path as the session list's interrupt keys.
async fn halt_seat(app: &mut App, hard: bool) {
    let Some((seat, id)) = selected_seat_session(app, "halt") else {
        return;
    };
    let status = app.sessions.get(&id).map(|s| s.session.status);
    let haltable = matches!(
        status,
        Some(SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval)
    );
    if !haltable {
        let shown = status.map_or_else(|| "not loaded".to_string(), |s| format!("{s:?}"));
        if let Some(workspace) = state(app) {
            workspace.error = Some(format!(
                "{} {} is {shown}; only a starting, running or waiting session can be halted",
                seat.label,
                short(id)
            ));
        }
        return;
    }
    app.interrupt_session_by_id(id, hard).await;
    if let Some(workspace) = state(app) {
        workspace.error = None;
        workspace.notice = Some(format!("Halt requested for {} {}", seat.label, short(id)));
    }
    refresh(app).await;
}

/// `a`: archive an earlier seat session. The current seat is refused:
/// rotate or revoke it first, so the console never archives a seat that
/// still holds authority.
async fn archive_seat(app: &mut App) {
    let Some((seat, id)) = selected_seat_session(app, "archive") else {
        return;
    };
    if !matches!(seat.row, WorkspaceRow::Predecessor { .. }) {
        if let Some(workspace) = state(app) {
            workspace.error = Some(format!(
                "{} is the current seat; revoke or replace it before archiving. Only earlier seat sessions can be archived here",
                seat.label
            ));
        }
        return;
    }
    app.archive_session_by_id(id).await;
    refresh(app).await;
}

async fn enter(app: &mut App) {
    let Some(workspace) = state(app) else { return };
    match workspace.selected_row() {
        Some(WorkspaceRow::Seat) => {
            match workspace.selected_seat().and_then(|seat| seat.session_id) {
                Some(id) => {
                    jump_to_session(app, id).await;
                }
                None if workspace.target.is_some() => {
                    workspace.error = Some(
                    "This node has no seat session to open; appoint one from the manager tree (a)"
                        .into(),
                );
                }
                None => {
                    workspace.error = Some(format!(
                        "No global manager session to open; {LAUNCH_HINT} or run {APPOINT_HINT}"
                    ));
                }
            }
        }
        Some(WorkspaceRow::Project(index)) => {
            let Some(project) = workspace.project(index) else {
                return;
            };
            let project_id = project.overview.project_id;
            match project.overview.manager.as_ref().map(|m| m.session_id) {
                Some(pm) => {
                    jump_to_session(app, pm).await;
                }
                // No live PM: the project's session list is the useful jump.
                None => {
                    jump_to_project(app, project_id);
                }
            }
        }
        Some(WorkspaceRow::MissingProject(_)) => {
            workspace.error = Some(format!(
                "That project no longer exists; re-appoint the grant without it ({LAUNCH_HINT})"
            ));
        }
        // #1627: a predecessor opens its earlier session like any seat.
        Some(WorkspaceRow::Predecessor { .. }) => {
            if let Some(id) = workspace.selected_seat().and_then(|seat| seat.session_id) {
                jump_to_session(app, id).await;
            }
        }
        // #1240: a child node opens its own console.
        Some(WorkspaceRow::Child(index)) => {
            if let Some(child) = workspace.child(index).map(|child| child.node) {
                open_node(app, child).await;
            }
        }
        None => {
            workspace.error = Some(format!(
                "Nothing to open: {LAUNCH_HINT}, or focus a session and run {APPOINT_HINT}"
            ));
        }
    }
}

fn open_project_tab(app: &mut App) {
    let Some(workspace) = state(app) else { return };
    let target = match workspace.selected_row() {
        Some(WorkspaceRow::Project(index)) => {
            workspace.project(index).map(|p| p.overview.project_id)
        }
        Some(WorkspaceRow::Seat | WorkspaceRow::Child(_) | WorkspaceRow::Predecessor { .. }) => {
            workspace.selected_seat().and_then(|seat| seat.project_id)
        }
        _ => None,
    };
    match target {
        Some(project_id) => {
            jump_to_project(app, project_id);
        }
        None => workspace.error = Some("This row has no project tab to open.".into()),
    }
}

/// Open the instantiate-a-manager form for the selected seat (`n`).
fn open_launch_form(app: &mut App) {
    let projects = app.projects.clone();
    let active_project = app.active_project_id();
    let Some(workspace) = state(app) else { return };
    if projects.is_empty() {
        workspace.error =
            Some("No projects exist yet; create one before appointing a manager".into());
        return;
    }
    let role = match workspace.selected_seat() {
        Some(SeatEntry {
            level: SeatLevel::Project,
            project_id: Some(id),
            ..
        }) => LaunchRole::Project(id),
        // #1240: a node console launches project managers only; portfolio
        // and area seats are appointed from the manager tree.
        _ if workspace.target.is_some() => {
            workspace.error = Some(
                "Here n launches a project manager; appoint portfolio and area seats from the manager tree (a)"
                    .into(),
            );
            return;
        }
        _ => LaunchRole::Global,
    };
    workspace.launch = Some(LaunchForm::new(
        role,
        &projects,
        workspace.grant(),
        active_project,
    ));
    workspace.focus = WorkspaceFocus::Seats;
    workspace.error = None;
    workspace.notice = None;
}

/// #1240: open the shown node's parent (`Backspace`); the global's console
/// has none above it here.
async fn open_parent(app: &mut App) {
    let Some(workspace) = state(app) else { return };
    let parent = workspace.node.as_ref().and_then(|node| node.parent);
    match parent {
        Some(parent) => open_node(app, parent).await,
        None => {
            workspace.error = Some("This console has no manager node above it here.".into());
        }
    }
}

/// Validate, launch and appoint from the open form. Errors stay inline in the
/// form; success closes it, refreshes and selects the new seat.
async fn submit_launch(app: &mut App, confirm_cap_reductions: bool) {
    let projects = app.projects.clone();
    let Some(workspace) = state(app) else { return };
    let Some(form) = workspace.launch.as_mut() else {
        return;
    };
    let request = match form.validate(&projects) {
        Ok(request) => request,
        Err(error) => {
            form.error = Some(error);
            return;
        }
    };
    let mut launched = form.launched;
    let result = launch_and_appoint(app, &request, &mut launched, confirm_cap_reductions).await;
    let Some(workspace) = state(app) else { return };
    match result {
        Ok((session_id, message)) => {
            workspace.launch = None;
            workspace.notice = Some(message);
            refresh(app).await;
            if let Some(workspace) = state(app) {
                let found = workspace.select_where(|seat| seat.session_id == Some(session_id))
                    || match request.role {
                        LaunchRole::Global => {
                            workspace.select_where(|seat| seat.level == SeatLevel::Global)
                        }
                        LaunchRole::Project(id) => {
                            workspace.select_where(|seat| seat.project_id == Some(id))
                        }
                    };
                if found {
                    ensure_conversation(app).await;
                }
            }
        }
        Err(error) => {
            if let Some(form) = workspace.launch.as_mut() {
                form.launched = launched;
                form.cap_confirm = error.contains(
                    rsi_common::portfolio_nodes::PORTFOLIO_CAP_REDUCTION_CONFIRMATION_REQUIRED,
                );
                form.error = Some(error);
            }
        }
    }
}

async fn handle_launch_key(app: &mut App, key: KeyEvent) {
    // Standard editing: a real cursor and selection in the launch prompt.
    let editing_prompt = state(app)
        .and_then(|w| w.launch.as_ref())
        .is_some_and(|form| {
            form.field == crate::overlay::global_manager_workspace_launch::LaunchField::Prompt
                && !form.cap_confirm
        });
    if editing_prompt {
        let result = app.edit_field(key, |overlay| match overlay {
            OverlayState::GlobalManagerWorkspace(w) => {
                w.launch.as_mut().map(|form| &mut form.prompt)
            }
            _ => None,
        });
        if result != crate::field_edit::FieldKey::Ignored {
            if let Some(form) = state(app).and_then(|w| w.launch.as_mut()) {
                if result == crate::field_edit::FieldKey::Edited {
                    form.prompt_edited = true;
                }
                form.error = None;
            }
            return;
        }
    }
    let Some(form) = state(app).and_then(|w| w.launch.as_mut()) else {
        return;
    };
    match handle_form_key(form, key) {
        FormOutcome::Stay => {}
        FormOutcome::Cancel => {
            let launched = form.launched;
            if let Some(workspace) = state(app) {
                workspace.launch = None;
                if let Some(id) = launched {
                    workspace.notice = Some(format!(
                        "Session {} was launched but not appointed; appoint it with :manager appoint or {APPOINT_HINT}",
                        short(id)
                    ));
                }
            }
        }
        FormOutcome::Submit => submit_launch(app, false).await,
        FormOutcome::ConfirmCaps => submit_launch(app, true).await,
    }
}

/// Lines a half-page conversation scroll moves.
fn half_page(app: &App, session_id: Uuid) -> usize {
    app.sessions
        .get(&session_id)
        .map_or(10, |s| (s.last_viewport_height / 2).max(1))
}

fn scroll_conversation(app: &mut App, direction: i32, lines: usize) {
    let Some(session_id) = open_conversation_session(app) else {
        return;
    };
    if let Some(session) = app.sessions.get_mut(&session_id) {
        crate::event::scroll_detail_lines(session, direction, lines);
    }
}

fn follow_tail(app: &mut App, follow: bool) {
    let Some(session_id) = open_conversation_session(app) else {
        return;
    };
    if let Some(session) = app.sessions.get_mut(&session_id) {
        session.follow_tail = follow;
        session.follow_tail_hold = false;
        if !follow {
            session.scroll_offset = 0;
            session.current_event_index = (!session.events.is_empty()).then_some(0);
        }
    }
}

/// Keys while the conversation transcript has focus.
fn handle_conversation_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers == KeyModifiers::CONTROL;
    let session = open_conversation_session(app);
    match key.code {
        KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab => {
            if let Some(workspace) = state(app) {
                workspace.focus = WorkspaceFocus::Seats;
            }
        }
        KeyCode::Char('i') if !ctrl => start_typing(app),
        KeyCode::Char('j') | KeyCode::Down => scroll_conversation(app, 1, 3),
        KeyCode::Char('k') | KeyCode::Up => scroll_conversation(app, -1, 3),
        KeyCode::Char('d') if ctrl => {
            let lines = session.map_or(10, |id| half_page(app, id));
            scroll_conversation(app, 1, lines);
        }
        KeyCode::Char('u') if ctrl => {
            let lines = session.map_or(10, |id| half_page(app, id));
            scroll_conversation(app, -1, lines);
        }
        KeyCode::PageDown => {
            let lines = session.map_or(10, |id| half_page(app, id) * 2);
            scroll_conversation(app, 1, lines);
        }
        KeyCode::PageUp => {
            let lines = session.map_or(10, |id| half_page(app, id) * 2);
            scroll_conversation(app, -1, lines);
        }
        KeyCode::Char('g') => follow_tail(app, false),
        KeyCode::Char('G') => follow_tail(app, true),
        KeyCode::Char('?') => crate::overlay::keybindings_help::open_contextual_help(app),
        _ => {}
    }
}

/// Keys while typing to the selected seat. Esc leaves typing (the draft
/// stays with that session); Enter or Ctrl-Enter sends.
async fn handle_input_key(app: &mut App, key: KeyEvent) {
    let Some(session_id) = open_conversation_session(app) else {
        if let Some(workspace) = state(app) {
            workspace.focus = WorkspaceFocus::Seats;
        }
        return;
    };
    let inserting = app
        .sessions
        .get(&session_id)
        .is_some_and(|s| s.input_bar.surface.mode == PopupMode::Insert);
    let consumed = crate::input_bar::handle_session_surface_key(app, session_id, key).await;
    let leave = key.code == KeyCode::Esc && (inserting || !consumed);
    if !consumed && key.modifiers.is_empty() {
        match key.code {
            KeyCode::Up => scroll_conversation(app, -1, 3),
            KeyCode::Down => scroll_conversation(app, 1, 3),
            _ => {}
        }
    }
    if leave {
        if let Some(workspace) = state(app) {
            workspace.focus = WorkspaceFocus::Seats;
        }
        return;
    }
    // A send clears the draft and leaves insert mode: keep typing.
    let still_inserting = app
        .sessions
        .get(&session_id)
        .is_some_and(|s| s.input_bar.surface.mode == PopupMode::Insert);
    if !still_inserting {
        crate::input_bar::enter_insert_mode(
            app,
            session_id,
            crate::modalkit_types::InsertStyle::Insert,
        );
    }
}

pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(workspace) = state(app) else { return };
    if workspace.launch.is_some() {
        handle_launch_key(app, key).await;
        app.mark_dirty();
        return;
    }
    match workspace.focus {
        WorkspaceFocus::Input => {
            handle_input_key(app, key).await;
            app.mark_dirty();
            return;
        }
        WorkspaceFocus::Conversation => {
            handle_conversation_key(app, key);
            app.mark_dirty();
            return;
        }
        WorkspaceFocus::Seats => {}
    }
    let len = workspace.rows().len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
        KeyCode::Char('r') => refresh(app).await,
        KeyCode::Enter | KeyCode::Char('l') => enter(app).await,
        KeyCode::Backspace => open_parent(app).await,
        KeyCode::Char('i') => start_typing(app),
        KeyCode::Char('t') => talk(app).await,
        KeyCode::Char('s') => seat_status(app),
        KeyCode::Char('x') => halt_seat(app, false).await,
        KeyCode::Char('X') => halt_seat(app, true).await,
        KeyCode::Char('a') => archive_seat(app).await,
        KeyCode::Char('n') => open_launch_form(app),
        KeyCode::Tab => {
            if workspace.conversation_session_id().is_some() {
                workspace.focus = WorkspaceFocus::Conversation;
            } else {
                workspace.error = Some(format!("This seat has no conversation; {LAUNCH_HINT}"));
            }
        }
        KeyCode::Char('p') => open_project_tab(app),
        KeyCode::Char('F') => crate::overlay::fleet::open(app).await,
        KeyCode::Char('T') => crate::overlay::manager_tree::open(app).await,
        KeyCode::Char(':') => crate::overlay::command_palette::open_command_palette(app),
        KeyCode::Char('?') => crate::overlay::keybindings_help::open_contextual_help(app),
        _ => {
            let mut selected = workspace.selected;
            if crate::overlay::list::handle_list_nav_key(&mut selected, len, &key)
                && selected != workspace.selected
            {
                workspace.selected = selected;
                workspace.error = None;
                ensure_conversation(app).await;
            }
        }
    }
    app.mark_dirty();
}

#[cfg(test)]
#[path = "global_manager_workspace_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "global_manager_workspace_node_tests.rs"]
pub(crate) mod node_tests;
