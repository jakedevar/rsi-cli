//! Operator policy and state surfaces over the shared, versioned manager API.
pub mod board;
pub mod catalog;
pub mod policy;

use crate::{app::App, modalkit_types::LcAction, types::OverlayState};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::{
    harness_manager::HarnessManagerConfigV1, harness_manager_v2::ManagerInspectSectionV2,
    types::Session,
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerSection {
    Board,
    Decisions,
    Inbox,
    Inspect(ManagerInspectSectionV2),
    Policy,
}

pub struct ManagerSurface {
    pub project_id: Uuid,
    pub identity: String,
    pub section: ManagerSection,
    pub ledger: board::BoardState,
    pub policy: Option<policy::PolicyState>,
    pub config: HarnessManagerConfigV1,
}

pub fn surface_mut(app: &mut App) -> Option<&mut ManagerSurface> {
    match &mut app.overlay {
        OverlayState::HarnessManagerV2(surface) => Some(surface),
        _ => None,
    }
}
pub fn board_mut(app: &mut App) -> Option<&mut board::BoardState> {
    surface_mut(app).map(|s| &mut s.ledger)
}
pub fn policy_mut(app: &mut App) -> Option<&mut policy::PolicyState> {
    surface_mut(app)?.policy.as_mut()
}

impl ManagerSection {
    pub fn inspect_section(self) -> ManagerInspectSectionV2 {
        match self {
            Self::Board => ManagerInspectSectionV2::Overview,
            Self::Decisions => ManagerInspectSectionV2::Decisions,
            Self::Inbox => ManagerInspectSectionV2::Requests,
            Self::Inspect(s) => s,
            Self::Policy => ManagerInspectSectionV2::Overview,
        }
    }
}

pub fn session_name(session: &Session) -> String {
    session
        .title
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| (!session.query.trim().is_empty()).then_some(session.query.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| session.id.to_string())
}

pub(crate) async fn open(app: &mut App, action: LcAction) {
    let section = match action {
        LcAction::EditHarnessManagerPolicy => ManagerSection::Policy,
        LcAction::OpenHarnessManagerDecisions => ManagerSection::Decisions,
        LcAction::OpenHarnessManagerInbox => ManagerSection::Inbox,
        LcAction::OpenHarnessManagerInspect => {
            ManagerSection::Inspect(ManagerInspectSectionV2::Workers)
        }
        _ => ManagerSection::Board,
    };
    let project = app
        .selected_session_state()
        .and_then(|s| s.session.project_id)
        .or(app.current_project_id);
    let Some(project) = project else {
        app.notify_error("Select a project with :projects first.");
        return;
    };
    match app.client.get_harness_manager(project).await {
        Ok(Some(config)) => {
            let identity = identity(app, &config);
            let result = board::open_section(app, config, identity, section).await;
            if let Err(error) = result {
                app.notify_error(error);
            }
        }
        Ok(None) => app.notify_error(
            "No manager appointed. Create a Standard root with :blank, then :manager appoint.",
        ),
        Err(error) => app.notify_error(format!("Manager: {error}")),
    }
    app.mark_dirty();
}

fn identity(app: &App, config: &HarnessManagerConfigV1) -> String {
    let id = config
        .current_session_id
        .unwrap_or(config.manager_session_id);
    let manager = app
        .sessions
        .get(&id)
        .map(|s| session_name(&s.session))
        .unwrap_or_else(|| id.to_string());
    let project = app
        .projects
        .iter()
        .find(|p| p.id == config.project_id)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| config.project_id.to_string());
    format!("{manager} · {project}")
}

pub(super) async fn handle_key(app: &mut App, key: KeyEvent) {
    let in_submode = surface_mut(app).is_some_and(|s| {
        s.ledger.answer.is_some()
            || s.policy
                .as_ref()
                .is_some_and(|p| p.edit.is_some() || p.picker.is_some())
    });
    if !in_submode {
        let section = surface_mut(app).map(|s| s.section);
        let direct = match key.code {
            KeyCode::Char('1') => Some(ManagerSection::Board),
            KeyCode::Char('2') => Some(ManagerSection::Decisions),
            KeyCode::Char('3') => Some(ManagerSection::Inbox),
            KeyCode::Char('4') => Some(ManagerSection::Inspect(ManagerInspectSectionV2::Workers)),
            KeyCode::Char('5') => Some(ManagerSection::Policy),
            _ => None,
        };
        if let Some(target) = direct {
            switch_section(app, target).await;
            app.mark_dirty();
            return;
        }
        if matches!(section, Some(ManagerSection::Inspect(_)))
            && matches!(key.code, KeyCode::Char(']' | '['))
        {
            let inspect = [
                ManagerInspectSectionV2::Workers,
                ManagerInspectSectionV2::Work,
                ManagerInspectSectionV2::Topology,
                ManagerInspectSectionV2::Resources,
                ManagerInspectSectionV2::Actions,
                ManagerInspectSectionV2::Events,
                ManagerInspectSectionV2::Archive,
                ManagerInspectSectionV2::Health,
            ];
            let current = match section {
                Some(ManagerSection::Inspect(s)) => {
                    inspect.iter().position(|x| *x == s).unwrap_or(0)
                }
                _ => 0,
            };
            let index = (current
                + if key.code == KeyCode::Char(']') {
                    1
                } else {
                    inspect.len() - 1
                })
                % inspect.len();
            switch_section(app, ManagerSection::Inspect(inspect[index])).await;
            app.mark_dirty();
            return;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let sections = [
                ManagerSection::Board,
                ManagerSection::Decisions,
                ManagerSection::Inbox,
                ManagerSection::Inspect(ManagerInspectSectionV2::Workers),
                ManagerSection::Policy,
            ];
            let index = sections
                .iter()
                .position(|s| match (s, section) {
                    (ManagerSection::Inspect(_), Some(ManagerSection::Inspect(_))) => true,
                    (s, section) => Some(*s) == section,
                })
                .unwrap_or(0);
            let index = (index
                + if key.code == KeyCode::Tab {
                    1
                } else {
                    sections.len() - 1
                })
                % sections.len();
            switch_section(app, sections[index]).await;
            app.mark_dirty();
            return;
        }
    }
    if surface_mut(app).is_some_and(|s| s.section == ManagerSection::Policy) {
        policy::handle_key(app, key).await;
    } else {
        board::handle_key(app, key).await;
    }
    app.mark_dirty();
}

#[allow(clippy::future_not_send)]
async fn switch_section(app: &mut App, section: ManagerSection) {
    let Some((project, config, identity)) =
        surface_mut(app).map(|s| (s.project_id, s.config.clone(), s.identity.clone()))
    else {
        return;
    };
    if section == ManagerSection::Policy && surface_mut(app).is_some_and(|s| s.policy.is_none()) {
        match policy::load(app, config, identity).await {
            Ok(state) => {
                if let Some(surface) = surface_mut(app) {
                    surface.policy = Some(state);
                }
            }
            Err(error) => {
                if let Some(surface) = surface_mut(app) {
                    surface.ledger.error = Some(error);
                }
                return;
            }
        }
    }
    if section == ManagerSection::Policy {
        // Every entry refreshes the header's usage in the background (#674).
        let socket = app.client.socket_path().to_path_buf();
        if let Some(state) = surface_mut(app).and_then(|s| s.policy.as_mut()) {
            state.start_usage(socket);
        }
    }
    if section == ManagerSection::Board {
        let result = board::fetch_board(app, project).await;
        if let Some(surface) = surface_mut(app) {
            match result {
                Ok((overview, pages)) => surface.ledger.install_board(overview, pages),
                Err(error) => {
                    surface.ledger.error = Some(format!("Manager section not loaded: {error}"));
                    return;
                }
            }
        }
    } else if section != ManagerSection::Policy {
        let query = rsi_common::harness_manager_v2::AgentManagerInspectRequestV2 {
            section: section.inspect_section(),
            ..Default::default()
        };
        let result = app
            .client
            .get_harness_manager_state(
                rsi_common::harness_manager_v2::GetHarnessManagerStateRequestV2 {
                    project_id: project,
                    query: query.clone(),
                },
            )
            .await;
        if let Some(surface) = surface_mut(app) {
            match result {
                Ok(page) => surface.ledger.install_page(query, vec![], page),
                Err(error) => {
                    surface.ledger.error = Some(format!("Manager section not loaded: {error}"));
                    return;
                }
            }
        }
    }
    if let Some(surface) = surface_mut(app) {
        surface.section = section;
    }
}

#[allow(clippy::future_not_send)]
pub(super) async fn reload_policy(app: &mut App) {
    let Some(project_id) = surface_mut(app).map(|s| s.project_id) else {
        return;
    };
    let config = match app.client.get_harness_manager(project_id).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            if let Some(state) = surface_mut(app).and_then(|s| s.policy.as_mut()) {
                state.error =
                    Some("Manager appointment is no longer available; draft retained.".into());
            }
            return;
        }
        Err(error) => {
            if let Some(state) = surface_mut(app).and_then(|s| s.policy.as_mut()) {
                state.error = Some(format!("Manager reload failed: {error}. Draft retained."));
            }
            return;
        }
    };
    let identity = identity(app, &config);
    match policy::load(app, config.clone(), identity.clone()).await {
        Ok(mut state) => {
            if let Some(surface) = surface_mut(app) {
                if let Some(previous) = surface.policy.as_mut() {
                    state.adopt_usage(previous);
                }
                surface.config = config;
                surface.identity = identity;
                surface.policy = Some(state);
            }
        }
        Err(error) => {
            if let Some(policy_state) = surface_mut(app).and_then(|s| s.policy.as_mut()) {
                policy_state.error =
                    Some(format!("Policy reload failed: {error}. Draft retained."));
            }
        }
    }
}

pub(crate) fn label_for(app: &App, id: Uuid) -> String {
    app.sessions
        .get(&id)
        .map(|s| session_name(&s.session))
        .unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests;
