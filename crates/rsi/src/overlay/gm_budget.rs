//! #2502: the Budget panel of the global manager workspace (`gm`, then `b`).
//!
//! One obvious place to see and raise a manager chain's budget: every cap
//! (created sessions, active sessions, containers, spend) of every node along
//! each covered project's chain (the portfolio nodes, then the project
//! manager) as used / limit, with the binding node per cap marked. `+` raises
//! the selected cap one step and a typed number sets it; both open a review
//! that names the grant it edits, and Enter saves through the existing
//! operator RPC (`ConfigurePortfolioNode` for a portfolio node,
//! `ConfigureHarnessManagerPolicy` for a PM) and reads the panel back.

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::manager_budget::{
    BudgetCapFieldV1, BudgetCapV1, BudgetNodeKindV1, BudgetNodeV1, GlobalManagerBudgetV1,
    raise_portfolio_request, raise_project_manager_request,
};
use uuid::Uuid;

use crate::app::App;
use crate::overlay::global_manager_workspace::GlobalManagerWorkspaceState;
use crate::types::OverlayState;

/// One selectable cap row: project, node in its chain, cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetRow {
    pub project: usize,
    pub node: usize,
    pub field: BudgetCapFieldV1,
}

/// A raise waiting for the operator's Enter.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRaise {
    pub project_name: String,
    pub kind: BudgetNodeKindV1,
    pub node_id: Uuid,
    pub node_label: String,
    pub field: BudgetCapFieldV1,
    pub from: Option<f64>,
    /// The limit as the daemon will store it (`None`: no cap, spend only):
    /// already normalized, so the review and the read-back compare the same
    /// value (#2523).
    pub to: Option<f64>,
    /// The fences the panel showed: grant/policy version and epoch/scope.
    pub version: i64,
    pub epoch: i64,
}

#[derive(Debug, Default)]
pub struct BudgetPanel {
    pub snapshot: Option<GlobalManagerBudgetV1>,
    pub selected: usize,
    /// A number being typed for the selected cap.
    pub entry: Option<String>,
    pub pending: Option<PendingRaise>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub loaded_at: Option<DateTime<Utc>>,
}

impl BudgetPanel {
    /// Every cap row, in display order.
    #[must_use]
    pub fn rows(&self) -> Vec<BudgetRow> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for (project, budget) in snapshot.projects.iter().enumerate() {
            for (node, entry) in budget.chain.iter().enumerate() {
                for cap in &entry.caps {
                    rows.push(BudgetRow {
                        project,
                        node,
                        field: cap.field,
                    });
                }
            }
        }
        rows
    }

    #[must_use]
    pub fn selected_row(&self) -> Option<BudgetRow> {
        self.rows().get(self.selected).copied()
    }

    /// The node and cap behind a row.
    #[must_use]
    pub fn resolve(&self, row: BudgetRow) -> Option<(&str, &BudgetNodeV1, &BudgetCapV1)> {
        let project = self.snapshot.as_ref()?.projects.get(row.project)?;
        let node = project.chain.get(row.node)?;
        Some((project.project_name.as_str(), node, node.cap(row.field)?))
    }

    /// Open the review of setting the selected cap to `to`.
    fn stage(&mut self, to: f64) {
        let Some(row) = self.selected_row() else {
            self.error = Some("No cap selected; press r to reload the panel".into());
            return;
        };
        let Some((project_name, node, cap)) = self.resolve(row) else {
            return;
        };
        let to = row.field.normalize(to);
        self.pending = Some(PendingRaise {
            project_name: project_name.to_string(),
            kind: node.kind,
            node_id: node.node_id,
            node_label: node.label.clone(),
            field: row.field,
            from: cap.limit,
            to,
            version: node.version,
            epoch: node.epoch,
        });
        self.entry = None;
        self.error = None;
    }

    /// `+`: one step above the current (or pending) limit. `+` is monotonic:
    /// an unlimited spend stays unlimited (a finite cap would reduce the
    /// allowance); type a number to install a finite cap (#2522).
    fn step_up(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            match pending.to {
                Some(to) => {
                    pending.to = pending
                        .field
                        .normalize((to + pending.field.step()).min(pending.field.max()));
                }
                None => self.notice = Some(UNLIMITED_SPEND_NOTICE.into()),
            }
            return;
        }
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(limit) = self.resolve(row).map(|(_, _, cap)| cap.limit) else {
            return;
        };
        match limit {
            Some(limit) => self.stage(limit + row.field.step()),
            None => {
                self.notice = Some(UNLIMITED_SPEND_NOTICE.into());
                self.error = None;
            }
        }
    }
}

const UNLIMITED_SPEND_NOTICE: &str = "Spend is unlimited here, and + never lowers an allowance: it is left unchanged. Type a number, then Enter, to install a finite cap";

fn workspace(app: &mut App) -> Option<&mut GlobalManagerWorkspaceState> {
    match &mut app.overlay {
        OverlayState::GlobalManagerWorkspace(state) => Some(state),
        _ => None,
    }
}

fn panel(app: &mut App) -> Option<&mut BudgetPanel> {
    workspace(app)?.budget.as_mut()
}

/// The portfolio node the workspace shows (`None`: the global node).
fn target_node(app: &mut App) -> Option<Uuid> {
    match workspace(app)?.target {
        Some(rsi_common::manager_tier_routing::ManagerNodeRefV1::Portfolio { node_id }) => {
            Some(node_id)
        }
        _ => None,
    }
}

/// `b` in the workspace: open the panel and load it.
pub async fn open(app: &mut App) {
    let Some(workspace) = workspace(app) else {
        return;
    };
    workspace.budget = Some(BudgetPanel::default());
    reload(app).await;
}

/// Re-read the panel. A failure keeps the last snapshot on screen.
pub async fn reload(app: &mut App) {
    let node = target_node(app);
    let result = app.client.get_global_manager_budget(node).await;
    let Some(panel) = panel(app) else { return };
    match result {
        Ok(snapshot) => {
            panel.snapshot = Some(snapshot);
            panel.loaded_at = Some(Utc::now());
            panel.selected = panel.selected.min(panel.rows().len().saturating_sub(1));
            panel.error = None;
        }
        Err(error) => {
            panel.error = Some(format!("Budget read failed: {error}; press r to retry"));
        }
    }
    app.mark_dirty();
}

/// Save the pending raise through the right operator RPC, then read back.
async fn commit(app: &mut App) {
    let Some(pending) = panel(app).and_then(|panel| panel.pending.clone()) else {
        return;
    };
    let key = Uuid::new_v4().to_string();
    let saved: Result<i64, String> = match pending.kind {
        BudgetNodeKindV1::Portfolio => commit_portfolio(app, &pending, key).await,
        BudgetNodeKindV1::Project => commit_project(app, &pending, key).await,
        BudgetNodeKindV1::Delegate => {
            Err("A delegate's allowance is edited in :manager tree (e on its row)".into())
        }
    };
    match saved {
        Ok(version) => {
            reload(app).await;
            let Some(panel) = panel(app) else { return };
            panel.pending = None;
            let read_back = panel.snapshot.as_ref().and_then(|snapshot| {
                snapshot
                    .projects
                    .iter()
                    .flat_map(|project| project.chain.iter())
                    .find(|node| node.kind == pending.kind && node.node_id == pending.node_id)
                    .and_then(|node| node.cap(pending.field))
                    .map(|cap| cap.limit)
            });
            match read_back {
                Some(limit) if limit == pending.to => {
                    panel.notice = Some(format!(
                        "Saved {} {} {}: {} → {} (v{version}); read back {}",
                        pending.kind.as_str(),
                        pending.node_label,
                        pending.field.label(),
                        describe(pending.field, pending.from),
                        describe(pending.field, pending.to),
                        describe(pending.field, limit),
                    ));
                }
                other => {
                    panel.error = Some(format!(
                        "Saved v{version}, but the panel reads back {} for {} (expected {}); press r",
                        describe(pending.field, other.flatten()),
                        pending.field.label(),
                        describe(pending.field, pending.to),
                    ));
                }
            }
        }
        Err(error) => {
            if let Some(panel) = panel(app) {
                panel.error = Some(error);
            }
        }
    }
    app.mark_dirty();
}

async fn commit_portfolio(
    app: &mut App,
    pending: &PendingRaise,
    key: String,
) -> Result<i64, String> {
    let node = app
        .client
        .get_portfolio_node(pending.node_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("The portfolio node is gone; press r to reload")?;
    if node.grant.grant_version != pending.version || node.authority_epoch != pending.epoch {
        return Err(format!(
            "The grant changed since the panel loaded (v{} → v{}); press r and retry",
            pending.version, node.grant.grant_version
        ));
    }
    let request = raise_portfolio_request(&node, pending.field, pending.to.unwrap_or(0.0), key);
    request.validate().map_err(str::to_string)?;
    // The review step is the operator's confirmation of a lowered cap.
    let saved = app
        .client
        .configure_portfolio_node_confirmed(request, true)
        .await
        .map_err(|error| format!("The daemon refused: {error}"))?;
    Ok(saved.grant.grant_version)
}

async fn commit_project(app: &mut App, pending: &PendingRaise, key: String) -> Result<i64, String> {
    let current = app
        .client
        .get_harness_manager_policy(pending.node_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("The project has no PM policy; press r to reload")?;
    if current.row_version != pending.version || current.scope_version != pending.epoch {
        return Err(format!(
            "The PM policy changed since the panel loaded (v{} → v{}); press r and retry",
            pending.version, current.row_version
        ));
    }
    let request =
        raise_project_manager_request(&current, pending.field, pending.to.unwrap_or(0.0), key);
    let saved = app
        .client
        .configure_harness_manager_policy(request)
        .await
        .map_err(|error| format!("The daemon refused: {error}"))?;
    Ok(saved.row_version)
}

/// A cap value as the panel prints it.
#[must_use]
pub fn describe(field: BudgetCapFieldV1, value: Option<f64>) -> String {
    match (field, value) {
        (_, None) => "none".to_string(),
        (BudgetCapFieldV1::SpendUsd, Some(value)) => format!("{value:.2}"),
        (_, Some(value)) => format!("{value:.0}"),
    }
}

/// Keys while the panel is open. Returns after marking the frame dirty.
pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(panel) = panel(app) else { return };
    if panel.pending.is_some() {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => commit(app).await,
            KeyCode::Char('+' | '=') => panel.step_up(),
            KeyCode::Esc | KeyCode::Char('n') => {
                panel.pending = None;
                panel.notice = Some("Raise cancelled; nothing was sent".into());
            }
            _ => {}
        }
        app.mark_dirty();
        return;
    }
    if let Some(entry) = panel.entry.as_mut() {
        match key.code {
            KeyCode::Char(ch) if ch.is_ascii_digit() || (ch == '.' && !entry.contains('.')) => {
                if entry.len() < 10 {
                    entry.push(ch);
                }
            }
            KeyCode::Backspace => {
                entry.pop();
            }
            KeyCode::Enter => match entry.parse::<f64>() {
                Ok(value) if value.is_finite() => panel.stage(value),
                _ => panel.error = Some("Type a number, then Enter".into()),
            },
            KeyCode::Esc => panel.entry = None,
            _ => {}
        }
        app.mark_dirty();
        return;
    }
    let len = panel.rows().len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q' | 'b') => {
            if let Some(workspace) = workspace(app) {
                workspace.budget = None;
            }
        }
        KeyCode::Char('r') => reload(app).await,
        KeyCode::Char('+' | '=') => panel.step_up(),
        KeyCode::Char(ch) if ch.is_ascii_digit() => {
            panel.entry = Some(ch.to_string());
            panel.notice = None;
            panel.error = None;
        }
        KeyCode::Char('?') => crate::overlay::keybindings_help::open_contextual_help(app),
        _ => {
            let mut selected = panel.selected;
            if crate::overlay::list::handle_list_nav_key(&mut selected, len, &key) {
                panel.selected = selected;
                panel.notice = None;
                panel.error = None;
            }
        }
    }
    app.mark_dirty();
}

#[cfg(test)]
#[path = "gm_budget_tests.rs"]
mod tests;
