//! Follow a project an agent created (#1626 slice 2).
//!
//! The daemon publishes `project_created` for `AgentCreateProject` only (the
//! operator's own `CreateProject` publishes nothing). The sync push handler
//! records the event; the event loop then runs [`App::settle_agent_created_project`]
//! to fetch the project, switch to its tab when the operator setting
//! `follow_agent_created_projects` is on, and offer the grant addition.

use rsi_common::agent_projects::{PROJECT_CREATED_SOURCE_AGENT, ProjectCreatedEventV1};
use uuid::Uuid;

use super::App;
use crate::settings::DaemonFeatureValue;

/// The daemon field behind the operator setting.
pub(crate) const FOLLOW_AGENT_CREATED_PROJECTS: &str = "follow_agent_created_projects";

impl App {
    /// Whether the TUI jumps to a project an agent just created. On until the
    /// daemon says otherwise.
    pub(crate) fn follow_agent_created_projects(&self) -> bool {
        self.daemon_features
            .iter()
            .find(|entry| entry.field == FOLLOW_AGENT_CREATED_PROJECTS)
            .is_none_or(|entry| !matches!(entry.value, DaemonFeatureValue::Bool(false)))
    }

    /// Record a `project_created` event for the async settle step. Anything
    /// that is not an agent create is ignored.
    pub(crate) fn note_agent_created_project(&mut self, event: ProjectCreatedEventV1) -> bool {
        if event.source != PROJECT_CREATED_SOURCE_AGENT {
            return false;
        }
        self.push_notification(
            crate::types::NotificationKind::Info,
            crate::types::NotificationPriority::Medium,
            format!("Agent created project \"{}\"", event.name),
            None,
        );
        self.pending_agent_project = Some(event);
        true
    }

    /// Switch to `project_id`'s tab, opening one when none exists.
    pub(crate) fn follow_project_tab(&mut self, project_id: Uuid) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.project_id == Some(project_id))
        {
            self.active_tab = index;
            self.sync_project_filter();
        } else {
            self.open_project_workspace(project_id);
        }
        self.mark_dirty();
    }

    /// Switch to the created project's tab when it is known and the operator
    /// setting is on. Returns whether the tab was followed.
    pub(crate) fn follow_created_project(&mut self, event: &ProjectCreatedEventV1) -> bool {
        if !self.projects.iter().any(|p| p.id == event.project_id)
            || !self.follow_agent_created_projects()
        {
            return false;
        }
        self.follow_project_tab(event.project_id);
        true
    }

    /// Run the deferred follow: refresh projects, switch tabs when the setting
    /// is on, then offer the creating global manager's grant addition.
    pub(crate) async fn settle_agent_created_project(&mut self) {
        let Some(event) = self.pending_agent_project.take() else {
            return;
        };
        if !self.poll.connected {
            return;
        }
        if let Ok(projects) = self.client.list_projects().await {
            let _ = self.update_projects(projects);
        }
        let followed = self.follow_created_project(&event);
        // Following off still leaves the grant offer for a project that exists.
        if !followed && !self.projects.iter().any(|p| p.id == event.project_id) {
            return;
        }
        crate::overlay::global_manager_command::offer_agent_project_grant(
            self,
            event.project_id,
            &event.name,
            event.created_by_session_id,
        )
        .await;
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use rsi_common::types::Project;
    use std::path::PathBuf;

    fn app() -> App {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-agent-created-project.sock",
        )))
    }

    fn project(name: &str) -> Project {
        let now = chrono::Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn event(project: &Project, source: &str) -> ProjectCreatedEventV1 {
        ProjectCreatedEventV1 {
            project_id: project.id,
            name: project.name.clone(),
            source: source.into(),
            created_by_session_id: Uuid::new_v4(),
        }
    }

    fn set_follow(app: &mut App, on: bool) {
        let entry = app
            .daemon_features
            .iter_mut()
            .find(|entry| entry.field == FOLLOW_AGENT_CREATED_PROJECTS)
            .expect("follow setting row");
        entry.value = DaemonFeatureValue::Bool(on);
    }

    fn active_project(app: &App) -> Option<Uuid> {
        app.tabs.get(app.active_tab).and_then(|tab| tab.project_id)
    }

    #[test]
    fn following_is_on_by_default() {
        assert!(app().follow_agent_created_projects());
    }

    #[test]
    fn the_push_event_records_an_agent_create_and_notifies() {
        let mut app = app();
        let made = project("Fresh");
        let redraw = app.apply_push_event(rsi_common::rpc::BusEvent {
            event_type: "project_created".into(),
            timestamp: chrono::Utc::now(),
            data: serde_json::to_value(event(&made, "agent")).unwrap(),
        });
        assert!(redraw);
        assert_eq!(
            app.pending_agent_project.as_ref().map(|e| e.project_id),
            Some(made.id)
        );
        assert_eq!(
            app.notifications.back().map(|n| n.message.clone()),
            Some("Agent created project \"Fresh\"".to_string())
        );
    }

    #[test]
    fn only_agent_creates_are_followed() {
        let mut app = app();
        let made = project("Operator made");
        assert!(!app.note_agent_created_project(event(&made, "operator")));
        assert!(app.pending_agent_project.is_none());
    }

    #[test]
    fn a_followed_create_switches_to_the_new_projects_tab() {
        let mut app = app();
        let made = project("Fresh");
        app.projects = vec![made.clone()];
        let tabs_before = app.tabs.len();
        assert!(app.follow_created_project(&event(&made, "agent")));
        assert_eq!(active_project(&app), Some(made.id));
        assert_eq!(app.tabs.len(), tabs_before + 1);
        // A second event for the same project reuses its tab.
        app.active_tab = 0;
        assert!(app.follow_created_project(&event(&made, "agent")));
        assert_eq!(active_project(&app), Some(made.id));
        assert_eq!(app.tabs.len(), tabs_before + 1);
    }

    #[test]
    fn with_the_setting_off_the_view_stays_put() {
        let mut app = app();
        let made = project("Fresh");
        app.projects = vec![made.clone()];
        set_follow(&mut app, false);
        assert!(!app.follow_agent_created_projects());
        let tab = app.active_tab;
        let tabs_before = app.tabs.len();
        assert!(!app.follow_created_project(&event(&made, "agent")));
        assert_eq!(app.active_tab, tab);
        assert_eq!(app.tabs.len(), tabs_before);
        set_follow(&mut app, true);
        assert!(app.follow_created_project(&event(&made, "agent")));
        assert_eq!(active_project(&app), Some(made.id));
    }
}
