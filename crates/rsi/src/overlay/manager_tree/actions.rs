//! In-tree operator actions for `:manager tree` (#1214).
//!
//! Each action is backed by an existing typed operator RPC; nothing here adds
//! an authority path:
//!
//! | row     | `a` appoint/replace            | `e` edit                          | `x` revoke                          |
//! |---------|--------------------------------|-----------------------------------|-------------------------------------|
//! | global  | `ConfigureGlobalManager` (seat)| `ConfigureGlobalManager` (projects)| `RevokeGlobalManager`              |
//! | portfolio | `ConfigurePortfolioNode` (seat) | `ConfigurePortfolioNode` (projects) | `RevokePortfolioNode` (grantor-scoped) |
//! | project | `ConfigureHarnessManager`      | `:manager scope` picker           | `ConfigureHarnessManager` (empty)   |
//! | area    | disabled: no seat-change RPC   | `ConfigureManagerNode` (allowance)| `RevokeManagerNode`                 |
//! | Epic    | disabled                       | disabled                          | disabled                            |
//!
//! #1237: on a portfolio row `A` appoints the focused session as a new
//! manager above the node (`ConfigurePortfolioNode` with `adopt_node_ids`),
//! and `m` marks the node, then `m` on a sibling moves it under that sibling
//! (the sibling widens to cover it and adopts it). Both preview the moved
//! subtree before `y`.
//!
//! `p` previews descendant impact from the loaded snapshot. Before a confirm
//! opens, the action reads the node's current version (`GetGlobalManager`,
//! `GetPortfolioNode`, `GetHarnessManager`, `GetManagerNode`); when it differs from the tree the
//! tree reloads instead of confirming against stale state. The RPC then
//! carries that version, so a change between confirm and commit is refused
//! by the daemon, and every outcome reloads the tree.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, GlobalManagerGrantV1, RevokeGlobalManagerRequestV1,
};
use rsi_common::harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerScopeModeV1};
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::manager_nodes::{
    ConfigureManagerNodeRequestV1, GetManagerNodeRequestV1, ManagerNodeSelectorV1,
    ManagerNodeViewV1, RevokeManagerNodeRequestV1,
};
use rsi_common::manager_tree::{ManagerTreeKindV1, ManagerTreeRowV1};
use rsi_common::portfolio_nodes::{
    ConfigurePortfolioNodeRequestV1, PortfolioNodeV1, RevokePortfolioNodeRequestV1,
};
use uuid::Uuid;

use super::{ManagerTreeState, Tone, close, kind_tag, load_text, refresh, set_notice, state};
use crate::app::App;
use crate::overlay::global_manager_command::{CapField, GrantCaps};

/// Affected seats a preview lists before it says how many it left out.
pub const IMPACT_LISTED_SEATS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeAction {
    Preview,
    Appoint,
    Edit,
    Revoke,
    /// #1237: appoint a new portfolio manager above this node.
    Above,
    /// #1237: move this node under a sibling (two presses).
    MoveUnder,
}

impl TreeAction {
    pub const ALL: [Self; 6] = [
        Self::Preview,
        Self::Appoint,
        Self::Edit,
        Self::Revoke,
        Self::Above,
        Self::MoveUnder,
    ];

    #[must_use]
    pub const fn key(self) -> char {
        match self {
            Self::Preview => 'p',
            Self::Appoint => 'a',
            Self::Edit => 'e',
            Self::Revoke => 'x',
            Self::Above => 'A',
            Self::MoveUnder => 'm',
        }
    }
}

/// The tier label a manager appointed above `label` gets: the next named
/// tier, else "above <label>" (display only; authority never reads it).
#[must_use]
pub fn label_above(label: &str) -> String {
    match label {
        "global" => "pinnacle".into(),
        "pinnacle" => "swarm".into(),
        other => format!("above {other}")
            .chars()
            .take(rsi_common::portfolio_nodes::PORTFOLIO_TIER_LABEL_MAX)
            .collect::<String>()
            .trim()
            .to_string(),
    }
}

/// The session focused behind the overlay; `a` appoints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: Uuid,
    pub name: String,
    pub project_id: Option<Uuid>,
    /// An unarchived leaf (the daemon refuses any other seat).
    pub eligible: bool,
}

/// Whether an action applies to a row now, and why not when it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionAvailability {
    pub action: TreeAction,
    pub label: String,
    pub disabled: Option<String>,
}

#[derive(Debug)]
pub enum TreeModal {
    Preview { title: String, lines: Vec<String> },
    Confirm(Box<PendingAction>),
    EditGlobal(Box<GlobalEditor>),
    EditNode(Box<NodeEditor>),
}

/// A prepared RPC waiting for the operator's explicit confirm.
#[derive(Debug, Clone)]
pub struct PendingAction {
    pub title: String,
    pub destructive: bool,
    /// What changes and the descendant impact, shown before confirming.
    pub lines: Vec<String>,
    /// The RPC and the versions it expects.
    pub rpc: String,
    pub request: PreparedRequest,
}

#[derive(Debug, Clone)]
pub enum PreparedRequest {
    ConfigureGlobal(ConfigureGlobalManagerRequestV1),
    ConfirmGlobalCaps(ConfigureGlobalManagerRequestV1),
    RevokeGlobal(RevokeGlobalManagerRequestV1),
    ConfigurePortfolio(Box<ConfigurePortfolioNodeRequestV1>),
    ConfirmPortfolioCaps(Box<ConfigurePortfolioNodeRequestV1>),
    RevokePortfolio(RevokePortfolioNodeRequestV1),
    ConfigureProject {
        request: ConfigureHarnessManagerRequestV1,
        revoke: bool,
    },
    ConfigureNode(Box<ConfigureManagerNodeRequestV1>),
    RevokeNode(RevokeManagerNodeRequestV1),
}

/// `e` on the global or a portfolio row: toggle the projects the grant covers
/// and edit the per-project caps (#1401). The cursor walks the projects, then
/// the cap rows.
#[derive(Debug)]
pub struct GlobalEditor {
    pub grant: GlobalManagerGrantV1,
    /// The portfolio node being edited (#1236); `None` for a pre-#1236
    /// global row, which saves through `ConfigureGlobalManager`.
    pub node: Option<Box<PortfolioNodeV1>>,
    /// (project, display name, granted)
    pub projects: Vec<(Uuid, String, bool)>,
    pub caps: GrantCaps,
    pub original_caps: GrantCaps,
    pub cursor: usize,
    pub error: Option<String>,
}

impl GlobalEditor {
    #[must_use]
    pub fn new(
        grant: GlobalManagerGrantV1,
        node: Option<Box<PortfolioNodeV1>>,
        projects: Vec<(Uuid, String, bool)>,
    ) -> Self {
        let caps = GrantCaps::from_policy(&grant.project_policy);
        Self {
            grant,
            node,
            projects,
            caps,
            original_caps: caps,
            cursor: 0,
            error: None,
        }
    }

    /// The cap row under the cursor, once it is past the projects.
    #[must_use]
    pub fn cap_field(&self) -> Option<CapField> {
        CapField::ALL
            .get(self.cursor.checked_sub(self.projects.len())?)
            .copied()
    }

    fn rows(&self) -> usize {
        self.projects.len() + CapField::ALL.len()
    }

    /// The policy this editor would save: the grant's with the edited caps.
    fn policy(&self) -> ManagerPolicyV2 {
        let mut policy = self.grant.project_policy.clone();
        self.caps.apply(&mut policy);
        policy
    }
}

pub const NODE_FIELDS: [&str; 3] = [
    "max active sessions",
    "max created sessions",
    "max direct reports",
];
const NODE_BOUNDS: [(u16, u16); 3] = [(1, 100), (0, 1024), (1, 64)];

/// `e` on an area node: carve its allowance (the daemon checks it stays
/// strictly narrower than the parent's).
#[derive(Debug)]
pub struct NodeEditor {
    pub row_label: String,
    pub node: ManagerNodeViewV1,
    pub parent: ManagerNodeViewV1,
    pub values: [u16; 3],
    pub original: [u16; 3],
    pub cursor: usize,
    pub error: Option<String>,
}

impl NodeEditor {
    pub fn adjust(&mut self, delta: i32) {
        let (low, high) = NODE_BOUNDS[self.cursor];
        let value = i32::from(self.values[self.cursor]) + delta;
        self.values[self.cursor] =
            u16::try_from(value.clamp(i32::from(low), i32::from(high))).unwrap_or(low);
        self.error = None;
    }
}

fn candidate_problem(
    candidate: Option<&Candidate>,
    current: Option<Uuid>,
    project: Option<Uuid>,
) -> Option<String> {
    let Some(candidate) = candidate else {
        return Some("focus the session to appoint in the session list first".into());
    };
    if !candidate.eligible {
        return Some(format!(
            "focused session \"{}\" is not an unarchived leaf",
            candidate.name
        ));
    }
    if current == Some(candidate.id) {
        return Some(format!("\"{}\" already holds this seat", candidate.name));
    }
    if project.is_some() && candidate.project_id != project {
        return Some(format!(
            "focused session \"{}\" belongs to another project",
            candidate.name
        ));
    }
    None
}

fn seat_label(base: &str, candidate: Option<&Candidate>, problem: Option<&String>) -> String {
    match (candidate, problem) {
        (Some(candidate), None) => format!("{base} → {}", candidate.name),
        _ => base.to_string(),
    }
}

/// What each action does on `row`, and why it is unavailable when it is.
#[must_use]
pub fn availability(
    row: &ManagerTreeRowV1,
    candidate: Option<&Candidate>,
) -> Vec<ActionAvailability> {
    let entry = |action, label: String, disabled: Option<String>| ActionAvailability {
        action,
        label,
        disabled,
    };
    let preview = entry(TreeAction::Preview, "preview impact".into(), None);
    let mut entries = match row.kind {
        ManagerTreeKindV1::Global | ManagerTreeKindV1::Portfolio => {
            let problem = candidate_problem(candidate, row.focus_session_id, None);
            let revoke = if row.kind == ManagerTreeKindV1::Global {
                "revoke global grant".to_string()
            } else {
                format!(
                    "revoke {} node",
                    row.tier_label.as_deref().unwrap_or("portfolio")
                )
            };
            vec![
                preview,
                entry(
                    TreeAction::Appoint,
                    seat_label("replace seat", candidate, problem.as_ref()),
                    problem,
                ),
                entry(TreeAction::Edit, "edit granted projects".into(), None),
                entry(TreeAction::Revoke, revoke, None),
            ]
        }
        ManagerTreeKindV1::Project => {
            let seated = row.focus_session_id.is_some();
            let problem = candidate_problem(candidate, row.focus_session_id, row.project_id);
            let no_manager = (!seated).then(|| "no manager appointed: appoint a seat first".into());
            vec![
                preview,
                entry(
                    TreeAction::Appoint,
                    seat_label(
                        if seated {
                            "replace seat"
                        } else {
                            "appoint seat"
                        },
                        candidate,
                        problem.as_ref(),
                    ),
                    problem,
                ),
                entry(TreeAction::Edit, "edit scope".into(), no_manager.clone()),
                entry(TreeAction::Revoke, "revoke supervision".into(), no_manager),
            ]
        }
        ManagerTreeKindV1::Area => {
            let revoked = row
                .grant
                .is_none()
                .then(|| "node is revoked: nothing to change".to_string());
            let edit = revoked.clone().or_else(|| {
                row.load
                    .direct_reports
                    .filter(|reports| *reports > 0)
                    .map(|reports| {
                        format!(
                            "node has {reports} direct report(s); the daemon only edits a childless node's grant"
                        )
                    })
            });
            vec![
                preview,
                entry(
                    TreeAction::Appoint,
                    "replace seat".into(),
                    Some("no RPC moves an area seat: revoke, then delegate anew".into()),
                ),
                entry(TreeAction::Edit, "edit allowance".into(), edit),
                entry(TreeAction::Revoke, "revoke subtree".into(), revoked),
            ]
        }
        ManagerTreeKindV1::Epic => {
            let none = |what: &str| Some(format!("an Epic lead is not a manager seat: {what}"));
            vec![
                preview,
                entry(
                    TreeAction::Appoint,
                    "replace lead".into(),
                    none("change the lead on the Epic"),
                ),
                entry(
                    TreeAction::Edit,
                    "edit".into(),
                    none("it has no grant or scope"),
                ),
                entry(
                    TreeAction::Revoke,
                    "revoke".into(),
                    none("revoke the manager above it"),
                ),
            ]
        }
    };
    // #1237: levels above a portfolio node are operator-created here.
    if row.kind == ManagerTreeKindV1::Portfolio {
        let problem = candidate_problem(candidate, row.focus_session_id, None);
        let label = format!(
            "appoint a {} manager above",
            label_above(row.tier_label.as_deref().unwrap_or("portfolio"))
        );
        entries.push(entry(
            TreeAction::Above,
            seat_label(&label, candidate, problem.as_ref()),
            problem,
        ));
        entries.push(entry(
            TreeAction::MoveUnder,
            "move under a sibling (m here, then m on the new parent)".into(),
            None,
        ));
    } else {
        let reason = || Some("only a portfolio node gains a manager above or moves".to_string());
        entries.push(entry(
            TreeAction::Above,
            "appoint a manager above".into(),
            reason(),
        ));
        entries.push(entry(TreeAction::MoveUnder, "move under".into(), reason()));
    }
    entries
}

/// Descendants of one row in the loaded snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Impact {
    /// Indices into `rows`, depth-first.
    pub descendants: Vec<usize>,
    pub projects: usize,
    pub areas: usize,
    pub epics: usize,
    /// Descendants holding a live seat session.
    pub seated: usize,
    /// The row or a descendant could not be traversed completely.
    pub incomplete: bool,
    /// The subtree reaches the end of the loaded pages while more rows exist.
    pub unloaded: bool,
}

impl ManagerTreeState {
    #[must_use]
    pub fn impact(&self, index: usize) -> Impact {
        let mut impact = Impact {
            incomplete: !self.rows[index].complete,
            ..Impact::default()
        };
        let mut members: HashSet<&str> = HashSet::from([self.rows[index].key.as_str()]);
        // Rows are a depth-first page: a subtree is contiguous.
        for (offset, row) in self.rows.iter().enumerate().skip(index + 1) {
            if !row
                .parent_key
                .as_deref()
                .is_some_and(|parent| members.contains(parent))
            {
                break;
            }
            members.insert(row.key.as_str());
            impact.descendants.push(offset);
            match row.kind {
                ManagerTreeKindV1::Project => impact.projects += 1,
                ManagerTreeKindV1::Area => impact.areas += 1,
                ManagerTreeKindV1::Epic => impact.epics += 1,
                ManagerTreeKindV1::Global | ManagerTreeKindV1::Portfolio => {}
            }
            impact.seated += usize::from(row.seat.is_some());
            impact.incomplete |= !row.complete;
        }
        let last = impact.descendants.last().copied().unwrap_or(index);
        impact.unloaded = self.next_after.is_some() && last + 1 == self.rows.len();
        impact
    }

    /// The descendant-impact text a preview and every confirm show.
    #[must_use]
    pub fn impact_lines(&self, index: usize) -> Vec<String> {
        let row = &self.rows[index];
        let impact = self.impact(index);
        let mut lines = vec![
            format!(
                "{} {}: {} descendant node(s) loaded: {} project(s), {} area node(s), {} Epic(s); {} with a live seat",
                kind_tag(row.kind),
                row.label,
                impact.descendants.len(),
                impact.projects,
                impact.areas,
                impact.epics,
                impact.seated,
            ),
            format!("This node: {}", load_text(row)),
        ];
        let seats: Vec<usize> = std::iter::once(index)
            .chain(impact.descendants.iter().copied())
            .filter(|i| self.rows[*i].seat.is_some())
            .collect();
        if seats.is_empty() {
            lines.push("Affected seats: none".into());
        } else {
            lines.push(format!("Affected seats ({}):", seats.len()));
            for i in seats.iter().take(IMPACT_LISTED_SEATS) {
                let seat_row = &self.rows[*i];
                lines.push(format!(
                    "  {} {} · {}",
                    kind_tag(seat_row.kind),
                    seat_row.label,
                    super::seat_text(seat_row)
                ));
            }
            if seats.len() > IMPACT_LISTED_SEATS {
                lines.push(format!(
                    "  … {} more affected seat(s) not listed",
                    seats.len() - IMPACT_LISTED_SEATS
                ));
            }
        }
        if impact.incomplete {
            lines.push("! Traversal incomplete under this node: counts are lower bounds.".into());
        }
        if impact.unloaded {
            lines.push(format!(
                "! {} more node(s) not loaded yet (n): this subtree may continue.",
                self.unloaded_rows()
            ));
        }
        lines
    }
}

fn short(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

fn project_name(app: &App, id: Uuid) -> String {
    app.projects
        .iter()
        .find(|project| project.id == id)
        .map_or_else(|| short(id), |project| project.name.clone())
}

/// The tree disagreed with the daemon: reload instead of confirming.
async fn stale(app: &mut App, what: &str) {
    refresh(app).await;
    set_notice(
        app,
        format!("The tree was stale ({what}); it has been refreshed. Review the node and retry."),
        Tone::Error,
    );
}

fn set_modal(app: &mut App, modal: TreeModal) {
    if let Some(tree) = state(app) {
        tree.modal = Some(modal);
        tree.notice = None;
    }
}

fn confirm(app: &mut App, pending: PendingAction) {
    set_modal(app, TreeModal::Confirm(Box::new(pending)));
}

/// Start `action` on the selected row: preview, open an editor, or prepare a
/// confirm against the node's current daemon version.
pub(super) async fn begin(app: &mut App, action: TreeAction) {
    let Some(tree) = state(app) else { return };
    let Some(index) = tree.selected_index() else {
        return;
    };
    let row = tree.rows[index].clone();
    let available = availability(&row, tree.candidate.as_ref());
    let Some(entry) = available.iter().find(|entry| entry.action == action) else {
        return;
    };
    if let Some(reason) = &entry.disabled {
        let text = format!("{}: unavailable: {reason}.", entry.label);
        set_notice(app, text, Tone::Error);
        return;
    }
    let impact = tree.impact_lines(index);
    let tree_version = tree.global_grant_version;
    let candidate = tree.candidate.clone();
    let result = match (action, row.kind) {
        (TreeAction::Preview, _) => {
            let mut lines = impact;
            lines.push(String::new());
            lines.push("Actions here:".into());
            for entry in &available {
                lines.push(match &entry.disabled {
                    None => format!("  {}  {}", entry.action.key(), entry.label),
                    Some(reason) => {
                        format!(
                            "  {}  {} (unavailable: {reason})",
                            entry.action.key(),
                            entry.label
                        )
                    }
                });
            }
            set_modal(
                app,
                TreeModal::Preview {
                    title: format!("Impact of {} {}", kind_tag(row.kind), row.label),
                    lines,
                },
            );
            Ok(())
        }
        (_, ManagerTreeKindV1::Global) => {
            global_action(app, action, tree_version, candidate, impact).await
        }
        (TreeAction::MoveUnder, ManagerTreeKindV1::Portfolio) => move_under_action(app, &row).await,
        (_, ManagerTreeKindV1::Portfolio) => {
            portfolio_action(app, action, &row, candidate, impact).await
        }
        (_, ManagerTreeKindV1::Project) => {
            project_action(app, action, &row, candidate, impact).await
        }
        (_, ManagerTreeKindV1::Area) => area_action(app, action, &row, impact).await,
        (_, ManagerTreeKindV1::Epic) => Ok(()),
    };
    if let Err(error) = result {
        set_notice(
            app,
            format!("{}: {error}", entry_label(&available, action)),
            Tone::Error,
        );
    }
}

fn entry_label(available: &[ActionAvailability], action: TreeAction) -> String {
    available
        .iter()
        .find(|entry| entry.action == action)
        .map_or_else(String::new, |entry| entry.label.clone())
}

async fn global_action(
    app: &mut App,
    action: TreeAction,
    tree_version: Option<i64>,
    candidate: Option<Candidate>,
    impact: Vec<String>,
) -> Result<(), String> {
    let current = app
        .client
        .get_global_manager()
        .await
        .map_err(|error| error.to_string())?;
    let Some(grant) = current.filter(|grant| Some(grant.grant_version) == tree_version) else {
        stale(app, "the global grant changed").await;
        return Ok(());
    };
    match action {
        TreeAction::Appoint => {
            let candidate = candidate.ok_or("focus the session to appoint")?;
            let request = ConfigureGlobalManagerRequestV1 {
                session_id: candidate.id,
                project_ids: grant.project_ids.clone(),
                allowed_launches: grant.allowed_launches.clone(),
                project_policy: grant.project_policy.clone(),
                expected_grant_version: grant.grant_version,
                idempotency_key: Uuid::new_v4().to_string(),
            };
            request.validate().map_err(str::to_string)?;
            let mut lines = vec![format!(
                "Seat: {} → \"{}\" ({}); projects, launches and PM policy unchanged.",
                short(grant.seat_session_id),
                candidate.name,
                short(candidate.id)
            )];
            lines.extend(impact);
            confirm(
                app,
                PendingAction {
                    title: "Replace the global manager seat".into(),
                    destructive: true,
                    lines,
                    rpc: format!(
                        "ConfigureGlobalManager (expects grant v{})",
                        grant.grant_version
                    ),
                    request: PreparedRequest::ConfigureGlobal(request),
                },
            );
        }
        TreeAction::Edit => {
            let projects = editor_projects(app, &grant);
            set_modal(
                app,
                TreeModal::EditGlobal(Box::new(GlobalEditor::new(grant, None, projects))),
            );
        }
        TreeAction::Revoke => {
            let mut lines = vec![format!(
                "Revokes the global grant v{} held by {}: the global seat loses authority over {} project(s).",
                grant.grant_version,
                short(grant.seat_session_id),
                grant.project_ids.len()
            )];
            lines.extend(impact);
            confirm(
                app,
                PendingAction {
                    title: "Revoke the global manager grant".into(),
                    destructive: true,
                    lines,
                    rpc: format!(
                        "RevokeGlobalManager (expects grant v{})",
                        grant.grant_version
                    ),
                    request: PreparedRequest::RevokeGlobal(RevokeGlobalManagerRequestV1 {
                        expected_grant_version: grant.grant_version,
                        idempotency_key: Uuid::new_v4().to_string(),
                    }),
                },
            );
        }
        TreeAction::Preview | TreeAction::Above | TreeAction::MoveUnder => {}
    }
    Ok(())
}

/// Every known project with whether `grant` covers it, plus granted ids the
/// TUI does not know, sorted by name.
fn editor_projects(app: &App, grant: &GlobalManagerGrantV1) -> Vec<(Uuid, String, bool)> {
    let mut projects: Vec<(Uuid, String, bool)> = app
        .projects
        .iter()
        .map(|project| {
            (
                project.id,
                project.name.clone(),
                grant.project_ids.contains(&project.id),
            )
        })
        .collect();
    for id in &grant.project_ids {
        if !projects.iter().any(|(project, ..)| project == id) {
            projects.push((*id, short(*id), true));
        }
    }
    projects.sort_by_cached_key(|(_, name, _)| name.to_lowercase());
    projects
}

/// The node's current fields under a new seat or project set, fenced on its
/// current grant version and epoch.
fn portfolio_request(
    node: &PortfolioNodeV1,
    seat: Uuid,
    project_ids: Vec<Uuid>,
) -> ConfigurePortfolioNodeRequestV1 {
    portfolio_request_with_policy(node, seat, project_ids, node.grant.project_policy.clone())
}

fn portfolio_request_with_policy(
    node: &PortfolioNodeV1,
    seat: Uuid,
    project_ids: Vec<Uuid>,
    policy: ManagerPolicyV2,
) -> ConfigurePortfolioNodeRequestV1 {
    ConfigurePortfolioNodeRequestV1 {
        node_id: Some(node.node_id),
        parent_node_id: node.parent_node_id,
        adopt_node_ids: Vec::new(),
        expected_parent_grant_version: None,
        tier_label: node.tier_label.clone(),
        seat_session_id: seat,
        project_ids,
        allowed_launches: node.grant.allowed_launches.clone(),
        policy,
        child_policy: node.child_policy.clone(),
        max_direct_reports: node.max_direct_reports,
        expected_node_grant_version: node.grant.grant_version,
        expected_authority_epoch: node.authority_epoch,
        idempotency_key: Uuid::new_v4().to_string(),
    }
}

/// #1236: actions on a portfolio node row, against the node's current grant
/// version (the row's `grant.grant_version`).
async fn portfolio_action(
    app: &mut App,
    action: TreeAction,
    row: &ManagerTreeRowV1,
    candidate: Option<Candidate>,
    impact: Vec<String>,
) -> Result<(), String> {
    let node_id = row.node_id.ok_or("the portfolio row carries no node")?;
    let current = app
        .client
        .get_portfolio_node(node_id)
        .await
        .map_err(|error| error.to_string())?;
    let tree_version = row.grant.as_ref().map(|grant| grant.grant_version);
    let Some(node) = current
        .filter(|node| node.state == "active" && Some(node.grant.grant_version) == tree_version)
    else {
        stale(app, "the node's grant changed").await;
        return Ok(());
    };
    let label = format!("{} node {}", node.tier_label, short(node.node_id));
    match action {
        TreeAction::Appoint => {
            let candidate = candidate.ok_or("focus the session to appoint")?;
            let request = portfolio_request(&node, candidate.id, node.grant.project_ids.clone());
            request.validate().map_err(str::to_string)?;
            let mut lines = vec![format!(
                "Seat: {} → \"{}\" ({}); projects, launches and policy unchanged; a new authority epoch opens.",
                short(node.grant.seat_session_id),
                candidate.name,
                short(candidate.id)
            )];
            lines.extend(impact);
            confirm(
                app,
                PendingAction {
                    title: format!("Replace the seat of the {label}"),
                    destructive: true,
                    lines,
                    rpc: format!(
                        "ConfigurePortfolioNode (expects grant v{} · epoch {})",
                        node.grant.grant_version, node.authority_epoch
                    ),
                    request: PreparedRequest::ConfigurePortfolio(Box::new(request)),
                },
            );
        }
        TreeAction::Edit => {
            let projects = editor_projects(app, &node.grant);
            set_modal(
                app,
                TreeModal::EditGlobal(Box::new(GlobalEditor::new(
                    node.grant.clone(),
                    Some(Box::new(node)),
                    projects,
                ))),
            );
        }
        TreeAction::Revoke => {
            let mut lines = vec![format!(
                "Revokes the {label} (grant v{}, seat {}): it loses authority over {} project(s) and releases their coverage.",
                node.grant.grant_version,
                short(node.grant.seat_session_id),
                node.grant.project_ids.len()
            )];
            lines.extend(impact);
            confirm(
                app,
                PendingAction {
                    title: format!("Revoke the {label}"),
                    destructive: true,
                    lines,
                    rpc: format!(
                        "RevokePortfolioNode (expects grant v{} · epoch {})",
                        node.grant.grant_version, node.authority_epoch
                    ),
                    request: PreparedRequest::RevokePortfolio(RevokePortfolioNodeRequestV1 {
                        node_id: node.node_id,
                        expected_grant_version: node.grant.grant_version,
                        expected_authority_epoch: node.authority_epoch,
                        idempotency_key: Uuid::new_v4().to_string(),
                    }),
                },
            );
        }
        TreeAction::Above => {
            let candidate = candidate.ok_or("focus the session to appoint")?;
            let above = label_above(&node.tier_label);
            let request = crate::overlay::portfolio_command::appoint_above_request(
                &above,
                candidate.id,
                &[&node],
                &[],
            )?;
            request.validate().map_err(str::to_string)?;
            let mut lines = vec![
                format!(
                    "Appoints \"{}\" ({}) as a new {above} manager over the {label}'s {} project(s), {}.",
                    candidate.name,
                    short(candidate.id),
                    node.grant.project_ids.len(),
                    match node.parent_node_id {
                        Some(parent) => format!("under its parent {}", short(parent)),
                        None => "as a root".into(),
                    }
                ),
                format!(
                    "The {label} and its descendants move one level down under new grant versions, keeping their seats, grantors, epochs, ledgers and workers; their queued mail is retired."
                ),
            ];
            lines.extend(impact);
            confirm(
                app,
                PendingAction {
                    title: format!("Appoint a {above} manager above the {label}"),
                    destructive: true,
                    lines,
                    rpc: format!(
                        "ConfigurePortfolioNode (adopts {} at grant v{})",
                        short(node.node_id),
                        node.grant.grant_version
                    ),
                    request: PreparedRequest::ConfigurePortfolio(Box::new(request)),
                },
            );
        }
        TreeAction::Preview | TreeAction::MoveUnder => {}
    }
    Ok(())
}

/// #1237 "move under": the first `m` marks the selected node; `m` on a
/// sibling prepares that sibling's widened grant adopting the marked node,
/// with the marked subtree's impact, behind `y`.
async fn move_under_action(app: &mut App, row: &ManagerTreeRowV1) -> Result<(), String> {
    let target_id = row.node_id.ok_or("the portfolio row carries no node")?;
    let Some(tree) = state(app) else {
        return Ok(());
    };
    let source = match tree.move_source.clone() {
        Some((source, _)) if source != target_id => source,
        _ => {
            tree.move_source = Some((target_id, row.label.clone()));
            set_notice(
                app,
                format!(
                    "Moving {}: select the sibling to move it under and press m (Esc cancels).",
                    row.label
                ),
                Tone::Info,
            );
            return Ok(());
        }
    };
    tree.move_source = None;
    let moved_impact = tree
        .rows
        .iter()
        .position(|candidate| candidate.node_id == Some(source))
        .map(|position| tree.impact_lines(position))
        .unwrap_or_default();
    let (Some(target), Some(moved)) = (
        app.client
            .get_portfolio_node(target_id)
            .await
            .map_err(|error| error.to_string())?,
        app.client
            .get_portfolio_node(source)
            .await
            .map_err(|error| error.to_string())?,
    ) else {
        stale(app, "a node of the move is gone").await;
        return Ok(());
    };
    if target.state != "active" || moved.state != "active" {
        stale(app, "a node of the move is revoked").await;
        return Ok(());
    }
    if target.parent_node_id != moved.parent_node_id {
        return Err(format!(
            "{} {} is not a sibling of {} {}: only a node with the same parent (or another root) adopts it",
            target.tier_label,
            short(target.node_id),
            moved.tier_label,
            short(moved.node_id)
        ));
    }
    let mut project_ids = target.grant.project_ids.clone();
    let added: Vec<Uuid> = moved
        .grant
        .project_ids
        .iter()
        .filter(|id| !project_ids.contains(id))
        .copied()
        .collect();
    project_ids.extend(&added);
    let mut request = portfolio_request(&target, target.grant.seat_session_id, project_ids);
    request.adopt_node_ids = vec![moved.node_id];
    request.policy =
        crate::overlay::portfolio_command::covering_policy(&target.grant.project_policy, &[&moved]);
    request.allowed_launches = crate::overlay::portfolio_command::covering_launches(
        &target.grant.allowed_launches,
        &[&moved],
    );
    request.max_direct_reports =
        crate::overlay::portfolio_command::covering_reports(target.max_direct_reports, &[&moved]);
    request.validate().map_err(str::to_string)?;
    let target_label = format!("{} node {}", target.tier_label, short(target.node_id));
    let moved_label = format!("{} node {}", moved.tier_label, short(moved.node_id));
    let mut lines = vec![
        format!(
            "The {target_label} widens to cover {} more project(s) [{}] and adopts the {moved_label}; its grant is widened only as far as the moved node needs, and a new authority epoch opens.",
            added.len(),
            added
                .iter()
                .map(|id| project_name(app, *id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        format!(
            "The {moved_label} and its descendants move one level down under new grant versions, keeping their seats, grantors, epochs, ledgers and workers; their queued mail is retired."
        ),
    ];
    lines.extend(moved_impact);
    confirm(
        app,
        PendingAction {
            title: format!("Move the {moved_label} under the {target_label}"),
            destructive: true,
            lines,
            rpc: format!(
                "ConfigurePortfolioNode (expects grant v{} · epoch {}; adopts {})",
                target.grant.grant_version,
                target.authority_epoch,
                short(moved.node_id)
            ),
            request: PreparedRequest::ConfigurePortfolio(Box::new(request)),
        },
    );
    Ok(())
}

async fn project_action(
    app: &mut App,
    action: TreeAction,
    row: &ManagerTreeRowV1,
    candidate: Option<Candidate>,
    impact: Vec<String>,
) -> Result<(), String> {
    let project_id = row.project_id.ok_or("the project row carries no project")?;
    if action == TreeAction::Edit {
        // The scope picker is the existing review-and-save surface.
        close(app);
        if let Err(error) =
            crate::overlay::harness_manager::edit_scope_for_project(app, project_id).await
        {
            app.notify_error(format!("Manager tree: edit scope: {error}"));
        }
        return Ok(());
    }
    let config = app
        .client
        .get_harness_manager(project_id)
        .await
        .map_err(|error| error.to_string())?;
    let live = config.as_ref().filter(|config| !config.is_revoked());
    if live.and_then(|config| config.current_session_id) != row.focus_session_id {
        stale(app, "the project seat changed").await;
        return Ok(());
    }
    let name = project_name(app, project_id);
    match action {
        TreeAction::Appoint => {
            let candidate = candidate.ok_or("focus the session to appoint")?;
            // Keep a live scope; a new or revoked seat starts on the whole project.
            let (epic_ids, group_ids, scope) = match live {
                Some(config) if config.scope_mode != HarnessManagerScopeModeV1::Project => (
                    Some(config.explicit_epic_ids().to_vec()),
                    config.group_ids.clone(),
                    format!(
                        "kept: {} group(s), {} epic(s)",
                        config.group_ids.len(),
                        config.explicit_epic_ids().len()
                    ),
                ),
                Some(_) => (None, Vec::new(), "kept: whole project".into()),
                None => (
                    None,
                    Vec::new(),
                    "whole project (no live scope to keep)".into(),
                ),
            };
            let expected_row_version = config.as_ref().map_or(0, |config| config.row_version);
            let mut lines = vec![
                format!(
                    "Seat: {} → \"{}\" ({}) for project {name}.",
                    row.focus_session_id.map_or_else(|| "none".into(), short),
                    candidate.name,
                    short(candidate.id)
                ),
                format!("Scope: {scope}."),
                "The daemon reports what the save does to the manager policy.".into(),
            ];
            lines.extend(impact);
            confirm(
                app,
                PendingAction {
                    title: if row.focus_session_id.is_some() {
                        format!("Replace the manager seat of {name}")
                    } else {
                        format!("Appoint a manager for {name}")
                    },
                    destructive: row.focus_session_id.is_some(),
                    lines,
                    rpc: format!("ConfigureHarnessManager (expects row v{expected_row_version})"),
                    request: PreparedRequest::ConfigureProject {
                        request: ConfigureHarnessManagerRequestV1 {
                            project_id,
                            session_id: candidate.id,
                            epic_ids,
                            group_ids,
                            expected_row_version,
                        },
                        revoke: false,
                    },
                },
            );
        }
        TreeAction::Revoke => {
            let config = live.ok_or("no manager appointed")?;
            let mut lines = vec![format!(
                "Clears the scope of {name}'s manager {}: supervision is revoked until a new scope is saved.",
                short(config.manager_session_id)
            )];
            lines.extend(impact);
            confirm(
                app,
                PendingAction {
                    title: format!("Revoke supervision of {name}"),
                    destructive: true,
                    lines,
                    rpc: format!(
                        "ConfigureHarnessManager, empty scope (expects row v{})",
                        config.row_version
                    ),
                    request: PreparedRequest::ConfigureProject {
                        request: ConfigureHarnessManagerRequestV1 {
                            project_id,
                            session_id: config.manager_session_id,
                            epic_ids: Some(Vec::new()),
                            group_ids: Vec::new(),
                            expected_row_version: config.row_version,
                        },
                        revoke: true,
                    },
                },
            );
        }
        TreeAction::Preview | TreeAction::Edit | TreeAction::Above | TreeAction::MoveUnder => {}
    }
    Ok(())
}

async fn area_action(
    app: &mut App,
    action: TreeAction,
    row: &ManagerTreeRowV1,
    impact: Vec<String>,
) -> Result<(), String> {
    let (Some(project_id), Some(node_id)) = (row.project_id, row.node_id) else {
        return Err("the area row carries no node".into());
    };
    let node = app
        .client
        .get_manager_node(GetManagerNodeRequestV1 {
            project_id,
            node_id,
        })
        .await
        .map_err(|error| error.to_string())?;
    let tree_version = row.grant.as_ref().map(|grant| grant.grant_version);
    let Some(node) = node.filter(|node| Some(node.grant_version) == tree_version) else {
        stale(app, "the node's grant changed").await;
        return Ok(());
    };
    match action {
        TreeAction::Revoke => {
            let mut lines = vec![format!(
                "Revokes area node {} (seat {}) and the authority it delegated below it.",
                short(node.node_id),
                short(node.seat_root_session_id)
            )];
            lines.extend(impact);
            confirm(
                app,
                PendingAction {
                    title: format!("Revoke {}", row.label),
                    destructive: true,
                    lines,
                    rpc: format!(
                        "RevokeManagerNode (expects grant v{} · epoch {})",
                        node.grant_version, node.authority_epoch
                    ),
                    request: PreparedRequest::RevokeNode(RevokeManagerNodeRequestV1 {
                        project_id,
                        node_id,
                        expected_grant_version: node.grant_version,
                        expected_authority_epoch: node.authority_epoch,
                        idempotency_key: Uuid::new_v4().to_string(),
                    }),
                },
            );
        }
        TreeAction::Edit => {
            let grant = node.grant.as_ref().ok_or("the node holds no grant")?;
            if node.direct_reports > 0 {
                stale(app, "the node gained direct reports").await;
                return Ok(());
            }
            let parent_id = node
                .parent_node_id
                .ok_or("the project root's grant is edited from the project, not here")?;
            let parent = app
                .client
                .get_manager_node(GetManagerNodeRequestV1 {
                    project_id,
                    node_id: parent_id,
                })
                .await
                .map_err(|error| error.to_string())?
                .ok_or("the parent node is unavailable")?;
            let values = [
                grant.allowance.max_active_sessions,
                grant.allowance.max_created_sessions,
                grant.max_direct_reports,
            ];
            set_modal(
                app,
                TreeModal::EditNode(Box::new(NodeEditor {
                    row_label: row.label.clone(),
                    node,
                    parent,
                    values,
                    original: values,
                    cursor: 0,
                    error: None,
                })),
            );
        }
        TreeAction::Preview | TreeAction::Appoint | TreeAction::Above | TreeAction::MoveUnder => {}
    }
    Ok(())
}

/// Turn a global editor into a confirm, or report why it cannot save.
fn confirm_global_edit(
    editor: &GlobalEditor,
    impact: Vec<String>,
) -> Result<PendingAction, String> {
    let project_ids: Vec<Uuid> = editor
        .projects
        .iter()
        .filter(|(_, _, granted)| *granted)
        .map(|(id, ..)| *id)
        .collect();
    if project_ids.is_empty() {
        return Err("Keep at least one project; x on the global row revokes the grant.".into());
    }
    let name = |id: &Uuid| {
        editor
            .projects
            .iter()
            .find(|(project, ..)| project == id)
            .map_or_else(|| short(*id), |(_, name, _)| name.clone())
    };
    let added: Vec<String> = project_ids
        .iter()
        .filter(|id| !editor.grant.project_ids.contains(id))
        .map(name)
        .collect();
    let removed: Vec<String> = editor
        .grant
        .project_ids
        .iter()
        .filter(|id| !project_ids.contains(id))
        .map(name)
        .collect();
    let caps_changed = editor.caps != editor.original_caps;
    if added.is_empty() && removed.is_empty() && !caps_changed {
        return Err("No change to save.".into());
    }
    let mut lines = Vec::new();
    for field in CapField::ALL {
        let (before, after) = (
            editor.original_caps.describe(field),
            editor.caps.describe(field),
        );
        if before != after {
            lines.push(format!("{}: {before} → {after}", field.label()));
        }
    }
    let narrowed = editor.caps.max_active_sessions < editor.original_caps.max_active_sessions
        || editor.caps.max_created_sessions < editor.original_caps.max_created_sessions
        || editor.caps.max_created_containers < editor.original_caps.max_created_containers;
    let scope = match (caps_changed, added.is_empty() && removed.is_empty()) {
        (true, true) => "caps",
        (true, false) => "projects and caps",
        (false, _) => "projects",
    };
    if !added.is_empty() {
        lines.push(format!("+ grant: {}", added.join(", ")));
    }
    if let Some(node) = &editor.node {
        if !removed.is_empty() {
            lines.push(format!(
                "- remove: {} (their seats leave this node's supervision)",
                removed.join(", ")
            ));
        }
        lines.extend(impact);
        let request = portfolio_request_with_policy(
            node,
            node.grant.seat_session_id,
            project_ids,
            editor.policy(),
        );
        request.validate().map_err(str::to_string)?;
        return Ok(PendingAction {
            title: format!("Edit the {} node's {scope}", node.tier_label),
            destructive: !removed.is_empty() || narrowed,
            lines,
            rpc: format!(
                "ConfigurePortfolioNode (expects grant v{} · epoch {})",
                node.grant.grant_version, node.authority_epoch
            ),
            request: PreparedRequest::ConfigurePortfolio(Box::new(request)),
        });
    }
    let request = ConfigureGlobalManagerRequestV1 {
        session_id: editor.grant.seat_session_id,
        project_ids,
        allowed_launches: editor.grant.allowed_launches.clone(),
        project_policy: editor.policy(),
        expected_grant_version: editor.grant.grant_version,
        idempotency_key: Uuid::new_v4().to_string(),
    };
    request.validate().map_err(str::to_string)?;
    if !removed.is_empty() {
        lines.push(format!(
            "- remove: {} (their seats leave global supervision)",
            removed.join(", ")
        ));
    }
    lines.extend(impact);
    Ok(PendingAction {
        title: format!("Edit the global grant's {scope}"),
        destructive: !removed.is_empty() || narrowed,
        lines,
        rpc: format!(
            "ConfigureGlobalManager (expects grant v{})",
            editor.grant.grant_version
        ),
        request: PreparedRequest::ConfigureGlobal(request),
    })
}

/// Turn a node editor into a confirm, or report why it cannot save.
fn confirm_node_edit(editor: &NodeEditor, impact: Vec<String>) -> Result<PendingAction, String> {
    if editor.values == editor.original {
        return Err("No change to save.".into());
    }
    let node = &editor.node;
    let mut grant = node.grant.clone().ok_or("the node holds no grant")?;
    let mut policy = node.policy.clone().ok_or("the node holds no policy")?;
    let selector = node
        .selector
        .clone()
        .unwrap_or(ManagerNodeSelectorV1::Project);
    let [active, created, reports] = editor.values;
    grant.allowance.max_active_sessions = active;
    grant.allowance.max_created_sessions = created;
    grant.max_direct_reports = reports;
    // The daemon requires the policy to mirror the grant's allowance.
    policy.max_active_sessions = active;
    policy.max_created_sessions = created;
    let request = ConfigureManagerNodeRequestV1 {
        node_id: Some(node.node_id),
        parent_node_id: editor.parent.node_id,
        project_id: node.project_id,
        seat_root_session_id: node.seat_root_session_id,
        selector,
        grant,
        policy,
        expected_parent_grant_version: editor.parent.grant_version,
        expected_parent_policy_version: editor.parent.policy_version,
        expected_parent_authority_epoch: editor.parent.authority_epoch,
        expected_node_grant_version: node.grant_version,
        idempotency_key: Uuid::new_v4().to_string(),
    };
    request.validate().map_err(str::to_string)?;
    let mut lines: Vec<String> = NODE_FIELDS
        .iter()
        .zip(editor.original.iter().zip(editor.values.iter()))
        .filter(|(_, (before, after))| before != after)
        .map(|(field, (before, after))| format!("{field}: {before} → {after}"))
        .collect();
    lines
        .push("The daemon refuses a grant that is not strictly narrower than the parent's.".into());
    lines.extend(impact);
    let narrowed = editor
        .values
        .iter()
        .zip(editor.original.iter())
        .any(|(after, before)| after < before);
    Ok(PendingAction {
        title: format!("Edit the allowance of {}", editor.row_label),
        destructive: narrowed,
        lines,
        rpc: format!(
            "ConfigureManagerNode (expects grant v{} · parent v{}/{}/epoch {})",
            node.grant_version,
            editor.parent.grant_version,
            editor.parent.policy_version,
            editor.parent.authority_epoch
        ),
        request: PreparedRequest::ConfigureNode(Box::new(request)),
    })
}

/// Keys while a preview, editor or confirm is open.
pub(super) async fn handle_modal_key(app: &mut App, key: KeyEvent) {
    let Some(tree) = state(app) else { return };
    let impact = tree
        .selected_index()
        .map(|index| tree.impact_lines(index))
        .unwrap_or_default();
    let Some(modal) = tree.modal.as_mut() else {
        return;
    };
    match modal {
        TreeModal::Preview { .. } => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'p')
            ) {
                tree.modal = None;
            }
        }
        TreeModal::Confirm(pending) => match key.code {
            KeyCode::Char('y') => commit(app).await,
            KeyCode::Enter if !pending.destructive => commit(app).await,
            KeyCode::Enter => {
                tree.notice = Some(super::Notice {
                    text: "This action is destructive: press y to confirm or Esc to cancel.".into(),
                    tone: Tone::Info,
                });
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                tree.modal = None;
                tree.notice = Some(super::Notice {
                    text: "Cancelled; nothing was sent.".into(),
                    tone: Tone::Info,
                });
            }
            _ => {}
        },
        TreeModal::EditGlobal(editor) => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => tree.modal = None,
            KeyCode::Char('j') | KeyCode::Down => {
                editor.cursor = (editor.cursor + 1).min(editor.rows().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => editor.cursor = editor.cursor.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(field) = editor.cap_field() {
                    if field == CapField::Groups {
                        let flip = if editor.caps.allow_create_groups {
                            -1
                        } else {
                            1
                        };
                        editor.caps.adjust(field, flip);
                        editor.error = None;
                    }
                } else if let Some(project) = editor.projects.get_mut(editor.cursor) {
                    project.2 = !project.2;
                    editor.error = None;
                }
            }
            KeyCode::Char('+' | 'l' | '-' | 'h' | 'L' | 'H') | KeyCode::Right | KeyCode::Left => {
                if let Some(field) = editor.cap_field() {
                    let delta = match key.code {
                        KeyCode::Char('+' | 'l') | KeyCode::Right => 1,
                        KeyCode::Char('L') => 10,
                        KeyCode::Char('H') => -10,
                        _ => -1,
                    };
                    editor.caps.adjust(field, delta);
                    editor.error = None;
                }
            }
            KeyCode::Enter => match confirm_global_edit(editor, impact) {
                Ok(pending) => tree.modal = Some(TreeModal::Confirm(Box::new(pending))),
                Err(error) => editor.error = Some(error),
            },
            _ => {}
        },
        TreeModal::EditNode(editor) => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => tree.modal = None,
            KeyCode::Char('j') | KeyCode::Down => {
                editor.cursor = (editor.cursor + 1).min(NODE_FIELDS.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => editor.cursor = editor.cursor.saturating_sub(1),
            KeyCode::Char('+' | 'l') | KeyCode::Right => editor.adjust(1),
            KeyCode::Char('-' | 'h') | KeyCode::Left => editor.adjust(-1),
            KeyCode::Enter => match confirm_node_edit(editor, impact) {
                Ok(pending) => tree.modal = Some(TreeModal::Confirm(Box::new(pending))),
                Err(error) => editor.error = Some(error),
            },
            _ => {}
        },
    }
}

/// A refused write is a preview: preserve its CAS fences and require `y`
/// after showing every affected project. No request is sent until that key.
pub(super) fn cap_confirmation_action(
    mut pending: PendingAction,
    error: &str,
) -> Option<PendingAction> {
    if !error.contains(rsi_common::portfolio_nodes::PORTFOLIO_CAP_REDUCTION_CONFIRMATION_REQUIRED) {
        return None;
    }
    pending.request = match pending.request {
        PreparedRequest::ConfigureGlobal(request) => PreparedRequest::ConfirmGlobalCaps(request),
        PreparedRequest::ConfigurePortfolio(request) => {
            PreparedRequest::ConfirmPortfolioCaps(request)
        }
        _ => return None,
    };
    pending.title = "Confirm lower project caps".into();
    pending.destructive = true;
    pending.lines = error.split("; ").map(str::to_string).collect();
    Some(pending)
}

/// #1544: a `:manager ... appoint` command refused for lower project caps
/// opens the tree on the same confirm step the tree's own appoint uses: the
/// refusal's preview, then `y` resends the identical request with
/// `confirm_cap_reductions:true`. Returns false (nothing opened) when `error`
/// is not a cap-reduction refusal or the tree cannot open.
pub(crate) async fn offer_cap_confirmation(
    app: &mut App,
    title: &str,
    request: PreparedRequest,
    error: &str,
) -> bool {
    let pending = PendingAction {
        title: title.into(),
        destructive: true,
        lines: Vec::new(),
        rpc: "ConfigureGlobalManager / ConfigurePortfolioNode".into(),
        request,
    };
    let Some(pending) = cap_confirmation_action(pending, error) else {
        return false;
    };
    super::open(app).await;
    if state(app).is_none() {
        return false;
    }
    confirm(app, pending);
    true
}

/// #1626: offer to add an agent-created project to the global grant. Opens the
/// tree on the usual confirm step: `y` sends the CAS-fenced request, Esc
/// leaves the grant alone. The agent never makes this change itself.
pub(crate) async fn offer_grant_addition(
    app: &mut App,
    title: String,
    lines: Vec<String>,
    request: ConfigureGlobalManagerRequestV1,
) -> bool {
    super::open(app).await;
    if state(app).is_none() {
        return false;
    }
    confirm(
        app,
        PendingAction {
            title,
            destructive: false,
            lines,
            rpc: "ConfigureGlobalManager".into(),
            request: PreparedRequest::ConfigureGlobal(request),
        },
    );
    true
}

/// Send the confirmed RPC, then reload the tree whatever the outcome.
async fn commit(app: &mut App) {
    let Some(tree) = state(app) else { return };
    let Some(TreeModal::Confirm(pending)) = tree.modal.take() else {
        return;
    };
    let retry = (*pending).clone();
    let PendingAction { title, request, .. } = *pending;
    let confirm_cap_reductions = matches!(
        &request,
        PreparedRequest::ConfirmGlobalCaps(_) | PreparedRequest::ConfirmPortfolioCaps(_)
    );
    let outcome: Result<String, String> = match request {
        PreparedRequest::ConfigureGlobal(request) | PreparedRequest::ConfirmGlobalCaps(request) => {
            app.client
                .configure_global_manager_confirmed(request, confirm_cap_reductions)
                .await
                .map(|grant| {
                    format!(
                        "Global grant saved as v{}: seat {}, {} project(s).",
                        grant.grant_version,
                        short(grant.seat_session_id),
                        grant.project_ids.len()
                    )
                })
                .map_err(|error| error.to_string())
        }
        PreparedRequest::RevokeGlobal(request) => app
            .client
            .revoke_global_manager(request)
            .await
            .map(|grant| format!("Global grant v{} revoked.", grant.grant_version))
            .map_err(|error| error.to_string()),
        PreparedRequest::ConfigurePortfolio(request)
        | PreparedRequest::ConfirmPortfolioCaps(request) => app
            .client
            .configure_portfolio_node_confirmed(*request, confirm_cap_reductions)
            .await
            .map(|node| {
                format!(
                    "{} node {} saved as grant v{} (epoch {}): seat {}, {} project(s).",
                    node.tier_label,
                    short(node.node_id),
                    node.grant.grant_version,
                    node.authority_epoch,
                    short(node.grant.seat_session_id),
                    node.grant.project_ids.len()
                )
            })
            .map_err(|error| error.to_string()),
        PreparedRequest::RevokePortfolio(request) => app
            .client
            .revoke_portfolio_node(request)
            .await
            .map(|node| format!("{} node {} revoked.", node.tier_label, short(node.node_id)))
            .map_err(|error| error.to_string()),
        PreparedRequest::ConfigureProject { request, revoke } => app
            .client
            .configure_harness_manager_outcome(request)
            .await
            .map(|saved| {
                format!(
                    "{} (row v{}). {}",
                    if revoke {
                        "Manager scope cleared; supervision revoked"
                    } else {
                        "Project manager seat saved"
                    },
                    saved.config.row_version,
                    crate::overlay::harness_manager::policy_outcome_note(&saved.policy)
                )
            })
            .map_err(|error| error.to_string()),
        PreparedRequest::ConfigureNode(request) => app
            .client
            .configure_manager_node(*request)
            .await
            .map(|node| {
                format!(
                    "Area node {} allowance saved as grant v{}.",
                    short(node.node_id),
                    node.grant_version
                )
            })
            .map_err(|error| error.to_string()),
        PreparedRequest::RevokeNode(request) => app
            .client
            .revoke_manager_node(request)
            .await
            .map(|node| format!("Area node {} revoked.", short(node.node_id)))
            .map_err(|error| error.to_string()),
    };
    if let Err(error) = &outcome
        && let Some(pending) = cap_confirmation_action(retry, error)
    {
        confirm(app, pending);
        return;
    }
    app.manager_roster.request_refresh();
    let refreshed = refresh(app).await;
    let suffix = if refreshed {
        " Tree refreshed."
    } else {
        " The tree could not refresh: press r."
    };
    match outcome {
        Ok(message) => set_notice(app, format!("{message}{suffix}"), Tone::Success),
        Err(error) if error.to_lowercase().contains("stale") => set_notice(
            app,
            format!(
                "Refused: the node changed after the preview ({error}).{suffix} Review and retry."
            ),
            Tone::Error,
        ),
        Err(error) => set_notice(
            app,
            format!("{title}: refused: {error}.{suffix}"),
            Tone::Error,
        ),
    }
}
