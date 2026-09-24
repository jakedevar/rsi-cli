//! Operator commands and draft scope selection for a project's harness manager.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::harness_manager::{
    ConfigureHarnessManagerRequestV1, HARNESS_MANAGER_MAX_EPICS, HARNESS_MANAGER_MAX_GROUPS,
    HarnessManagerConfigV1, HarnessManagerScopeCandidateV1, HarnessManagerScopeModeV1,
    ListHarnessManagerScopeRequestV1,
};
use rsi_common::types::{Session, SessionKind, SessionStatus};
use uuid::Uuid;

use crate::app::App;
use crate::client::ClientError;
use crate::modalkit_types::LcAction;
use crate::types::OverlayState;

const NO_PROJECT: &str =
    "No project selected. Use :projects or focus a session assigned to a project.";
const NO_MANAGER: &str = "No manager appointed. Focus an ordinary leaf and use :manager appoint.";

#[derive(Debug)]
pub struct HarnessManagerScopeRow {
    pub id: Uuid,
    pub name: String,
    pub group_name: String,
    pub group_id: Option<Uuid>,
    pub kind: SessionKind,
    pub available: bool,
}

#[derive(Debug)]
pub struct HarnessManagerScopeState {
    pub project_id: Uuid,
    pub project_name: String,
    pub manager_name: String,
    pub manager_session_id: Uuid,
    pub expected_row_version: i64,
    pub appointing: bool,
    pub rows: Vec<HarnessManagerScopeRow>,
    pub selected_epics: HashSet<Uuid>,
    pub selected_groups: HashSet<Uuid>,
    pub all_project: bool,
    pub selected: usize,
    pub query: String,
    pub searching: bool,
    pub error: Option<String>,
}

impl HarnessManagerScopeState {
    pub fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, epic)| {
                epic.name.to_lowercase().contains(&query)
                    || epic.group_name.to_lowercase().contains(&query)
                    || epic.id.to_string().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn request(&self) -> ConfigureHarnessManagerRequestV1 {
        let mut epic_ids: Vec<_> = self.selected_epics.iter().copied().collect();
        epic_ids.sort_unstable();
        let mut group_ids: Vec<_> = self.selected_groups.iter().copied().collect();
        group_ids.sort_unstable();
        ConfigureHarnessManagerRequestV1 {
            project_id: self.project_id,
            session_id: self.manager_session_id,
            epic_ids: (!self.all_project).then_some(epic_ids),
            group_ids,
            expected_row_version: self.expected_row_version,
        }
    }

    pub fn inherited(&self, row: &HarnessManagerScopeRow) -> bool {
        self.all_project
            || row
                .group_id
                .is_some_and(|id| self.selected_groups.contains(&id))
    }

    pub fn chosen(&self, row: &HarnessManagerScopeRow) -> bool {
        if row.kind == SessionKind::Group {
            self.selected_groups.contains(&row.id)
        } else {
            self.selected_epics.contains(&row.id)
        }
    }

    fn select_project(&mut self) {
        self.all_project = true;
        self.selected_epics.clear();
        self.selected_groups.clear();
        self.error = None;
    }

    fn toggle_selected(&mut self) {
        let Some(&index) = self.visible_indices().get(self.selected) else {
            return;
        };
        let row = &self.rows[index];
        let groups = row.kind == SessionKind::Group;
        let selected = if groups {
            &mut self.selected_groups
        } else {
            &mut self.selected_epics
        };
        if selected.remove(&row.id) {
            self.error = None;
            return;
        }
        if !row.available {
            self.error = Some("Selection unavailable. Reopen :manager scope to refresh.".into());
            return;
        }
        if !groups
            && row
                .group_id
                .is_some_and(|id| self.selected_groups.contains(&id))
        {
            self.error = Some(
                "Covered by selected Group. Deselect Group to choose individual Epics.".into(),
            );
            return;
        }
        let (count, limit, label) = if groups {
            (
                self.selected_groups.len(),
                HARNESS_MANAGER_MAX_GROUPS,
                "Groups",
            )
        } else {
            (
                self.selected_epics.len(),
                HARNESS_MANAGER_MAX_EPICS,
                "individual Epics",
            )
        };
        if count >= limit {
            self.error = Some(format!("Select up to {limit} {label}. Deselect one first."));
            return;
        }
        self.all_project = false;
        if groups {
            self.selected_groups.insert(row.id);
            for epic in &self.rows {
                if epic.group_id == Some(row.id) {
                    self.selected_epics.remove(&epic.id);
                }
            }
        } else {
            self.selected_epics.insert(row.id);
        }
        self.error = None;
    }
}

fn session_name(session: &Session) -> String {
    session
        .title
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&session.query)
        .to_string()
}

fn available(session: &Session) -> bool {
    !matches!(
        session.status,
        SessionStatus::Archived | SessionStatus::Deleted
    )
}

fn rpc_error(error: ClientError, appointing: bool) -> String {
    match error {
        ClientError::Rpc { code: -32601, .. } => {
            "Daemon lacks harness manager support. Update/restart rsid, then retry :manager.".into()
        }
        ClientError::Rpc { ref message, .. } if message.to_lowercase().contains("stale") => {
            let command = if appointing { "appoint" } else { "scope" };
            format!("Scope changed. Esc, reopen :manager {command}, then review and save.")
        }
        ClientError::NotConnected | ClientError::ConnectionClosed => {
            "Daemon disconnected. Reconnect to rsid, then retry :manager.".into()
        }
        other => format!(
            "Manager: {other}. Reopen :manager scope to refresh; check the selected session and Epics."
        ),
    }
}

pub(crate) async fn dispatch(app: &mut App, action: LcAction) {
    if let Err(error) = run_command(app, action).await {
        app.notify_error(error);
    }
    app.mark_dirty();
}

async fn run_command(app: &mut App, action: LcAction) -> Result<(), String> {
    let appointing = matches!(action, LcAction::AppointHarnessManager);
    let focused = app.selected_session_state().map(|state| &state.session);
    let candidate = if appointing {
        let session = focused.ok_or(NO_MANAGER)?;
        if !rsi_common::is_leaf_kind(session.session_kind) || !available(session) {
            return Err(
                "Focus an ordinary, unarchived leaf session, then use :manager appoint.".into(),
            );
        }
        Some((session.id, session.project_id.ok_or(NO_PROJECT)?))
    } else {
        None
    };
    let project_id = candidate
        .map(|(_, project)| project)
        .or_else(|| {
            focused
                .and_then(|session| session.project_id)
                .or(app.current_project_id)
        })
        .ok_or(NO_PROJECT)?;
    let config = app
        .client
        .get_harness_manager(project_id)
        .await
        .map_err(|error| rpc_error(error, appointing))?;

    match action {
        LcAction::AppointHarnessManager | LcAction::EditHarnessManagerScope => {
            let manager_id = match candidate {
                Some((id, _)) => id,
                None => config.as_ref().ok_or(NO_MANAGER)?.manager_session_id,
            };
            let mut candidates = Vec::new();
            let mut after_id = None;
            loop {
                let page = app
                    .client
                    .list_harness_manager_scope(ListHarnessManagerScopeRequestV1 {
                        project_id,
                        after_id,
                        limit: 64,
                    })
                    .await
                    .map_err(|error| rpc_error(error, appointing))?;
                candidates.extend(page.rows);
                // Bound the draft without silently presenting a partial picker.
                if candidates.len() > 4096 {
                    return Err("Manager picker exceeds 4096 Groups/Epics. Configure project scope through the operator API.".into());
                }
                match page.next_after_id {
                    Some(next) if after_id.is_none_or(|previous| next > previous) => after_id = Some(next),
                    Some(_) => return Err("Manager scope discovery returned a nonadvancing cursor. Reopen :manager scope.".into()),
                    None => break,
                }
            }
            open_scope(app, project_id, manager_id, config, appointing, candidates).await;
        }
        LcAction::ClearHarnessManagerScope => {
            let config = config.ok_or(NO_MANAGER)?;
            app.client
                .configure_harness_manager(ConfigureHarnessManagerRequestV1 {
                    group_ids: Vec::new(),
                    project_id: config.project_id,
                    session_id: config.manager_session_id,
                    epic_ids: Some(Vec::new()),
                    expected_row_version: config.row_version,
                })
                .await
                .map_err(|error| rpc_error(error, false))?;
            app.manager_roster.request_refresh();
            app.notify_success("Manager scope cleared; supervision revoked.");
        }
        LcAction::OpenHarnessManager => open_manager(app, config.ok_or(NO_MANAGER)?).await?,
        _ => unreachable!("only manager actions are dispatched here"),
    }
    Ok(())
}

async fn open_scope(
    app: &mut App,
    project_id: Uuid,
    manager_session_id: Uuid,
    config: Option<HarnessManagerConfigV1>,
    appointing: bool,
    candidates: Vec<HarnessManagerScopeCandidateV1>,
) {
    let prior_scope = config.as_ref().filter(|config| {
        !appointing
            || config.manager_session_id == manager_session_id
            || config.current_session_id == Some(manager_session_id)
    });
    let selected_epics: HashSet<_> = prior_scope
        .map(|config| config.explicit_epic_ids().iter().copied().collect())
        .unwrap_or_default();
    let selected_groups: HashSet<_> = prior_scope
        .map(|config| config.group_ids.iter().copied().collect())
        .unwrap_or_default();
    let all_project =
        prior_scope.is_none_or(|config| config.scope_mode == HarnessManagerScopeModeV1::Project);
    let mut sessions: HashMap<_, _> = app
        .sessions
        .iter()
        .map(|(id, state)| (*id, state.session.clone()))
        .collect();
    // Only explicit selections need historical identity recovery. Expanded
    // membership is not saved back as individual Epic selections.
    for id in selected_epics
        .iter()
        .chain(&selected_groups)
        .chain(std::iter::once(&manager_session_id))
    {
        if !sessions.contains_key(id)
            && let Ok(session) = app.client.get_session(*id).await
        {
            sessions.insert(*id, session);
        }
    }
    let mut rows: Vec<_> = candidates
        .into_iter()
        .map(|row| HarnessManagerScopeRow {
            id: row.id,
            name: row.title,
            kind: row.kind,
            group_id: row.group_id,
            group_name: row.group_title.unwrap_or_default(),
            available: true,
        })
        .collect();
    for (selection, kind) in [
        (&selected_groups, SessionKind::Group),
        (&selected_epics, SessionKind::Epic),
    ] {
        for id in selection {
            if !rows.iter().any(|row| row.id == *id) {
                rows.push(HarnessManagerScopeRow {
                    id: *id,
                    kind,
                    name: sessions
                        .get(id)
                        .map(session_name)
                        .unwrap_or_else(|| id.to_string()),
                    group_name: String::new(),
                    group_id: None,
                    available: false,
                });
            }
        }
    }
    rows.sort_by_cached_key(|row| {
        (
            if row.kind == SessionKind::Group {
                row.name.to_lowercase()
            } else {
                row.group_name.to_lowercase()
            },
            row.group_id.unwrap_or(row.id),
            row.kind != SessionKind::Group,
            row.name.to_lowercase(),
            row.id,
        )
    });
    app.overlay_leader_pending = false;
    app.overlay = OverlayState::HarnessManagerScope(Box::new(HarnessManagerScopeState {
        project_id,
        project_name: app
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.name.clone())
            .unwrap_or_else(|| project_id.to_string()),
        manager_name: sessions
            .get(&manager_session_id)
            .map(session_name)
            .unwrap_or_else(|| manager_session_id.to_string()),
        manager_session_id,
        expected_row_version: config.map_or(0, |config| config.row_version),
        appointing,
        rows,
        selected_epics,
        selected_groups,
        all_project,
        selected: 0,
        query: String::new(),
        searching: false,
        error: None,
    }));
}

/// Follow only rotation lineage, never hierarchy or a feature Epic's lead.
fn manager_tip(
    sessions: &HashMap<Uuid, Session>,
    config: &HarnessManagerConfigV1,
) -> Result<Uuid, String> {
    let invalid = "Manager lineage is unavailable or ambiguous. Focus the intended leaf and use :manager appoint.";
    // The daemon distinguishes committed turnover from a reserved/Fresh
    // candidate. Reconstructing authority from a client session list cannot.
    let id = config.current_session_id.ok_or(invalid)?;
    let session = sessions.get(&id).ok_or(invalid)?;
    if session.project_id != Some(config.project_id)
        || !rsi_common::is_leaf_kind(session.session_kind)
        || !available(session)
    {
        return Err(invalid.into());
    }
    Ok(id)
}

async fn open_manager(app: &mut App, config: HarnessManagerConfigV1) -> Result<(), String> {
    let target = config.current_session_id.ok_or(
        "Manager lineage is unavailable or ambiguous. Focus the intended leaf and use :manager appoint.",
    )?;
    let current = app
        .client
        .get_session(target)
        .await
        .map_err(|error| rpc_error(error, false))?;
    let mut sessions = HashMap::from([(current.id, current)]);
    let id = manager_tip(&sessions, &config)?;
    app.upsert_session(sessions.remove(&id).expect("resolved manager exists"));
    app.detail_list_focused = false;
    app.open_session_in_current_pane(id);
    if config.is_revoked() {
        app.notify(
            "Manager scope is empty; use :manager scope to select project, Groups, or Epics.",
        );
    }
    Ok(())
}

pub(super) async fn handle_key(app: &mut App, key: KeyEvent) {
    let OverlayState::HarnessManagerScope(state) = &mut app.overlay else {
        return;
    };
    if state.searching {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => state.searching = false,
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                state.query.push(c);
                state.selected = 0;
            }
            _ => {}
        }
    } else {
        let visible_count = state.visible_indices().len();
        if super::list::handle_list_nav_key(&mut state.selected, visible_count, &key) {
            app.mark_dirty();
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
            KeyCode::Char('/') => {
                state.searching = true;
                state.query.clear();
                state.selected = 0;
            }
            KeyCode::Char(' ') => state.toggle_selected(),
            KeyCode::Char('a') => state.select_project(),
            KeyCode::Enter => {
                let request = state.request();
                if state
                    .rows
                    .iter()
                    .any(|epic| !state.all_project && !epic.available && state.chosen(epic))
                {
                    state.error = Some("Deselect unavailable Groups/Epics before saving.".into());
                } else if let Err(error) = request.validate() {
                    state.error = Some(format!(
                        "Invalid scope: {error}. Select project, Groups, or up to {HARNESS_MANAGER_MAX_EPICS} individual Epics."
                    ));
                } else {
                    let appointing = state.appointing;
                    let expected_row_version = state.expected_row_version;
                    match app.client.configure_harness_manager(request).await {
                        Ok(config) => {
                            app.overlay = OverlayState::None;
                            if config.row_version == expected_row_version {
                                app.notify_success(
                                    "Manager scope unchanged; no policy or watches revoked.",
                                );
                            } else {
                                app.manager_roster.request_refresh();
                                let selection =
                                    if config.scope_mode == HarnessManagerScopeModeV1::Project {
                                        "whole project, including future Epics".to_string()
                                    } else {
                                        format!(
                                            "{} Groups + {} individual Epics",
                                            config.group_ids.len(),
                                            config.explicit_epic_ids().len()
                                        )
                                    };
                                app.notify_success(format!("Manager scope saved: {selection} ({} Epics now). Use :manager to open the conversation.", config.epic_ids.len()));
                            }
                        }
                        Err(error) => {
                            if let OverlayState::HarnessManagerScope(state) = &mut app.overlay {
                                state.error = Some(rpc_error(error, appointing));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    app.mark_dirty();
}

#[cfg(test)]
mod tests;
