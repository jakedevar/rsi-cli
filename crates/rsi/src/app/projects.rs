//! Project filtering operations for App.

use super::App;
use crate::state::PersistedState;
use crate::types::{Pane, SplitNode};
use rsi_common::types::Project;
use uuid::Uuid;

impl App {
    /// Get the project ID of the active workspace.
    pub fn active_project_id(&self) -> Option<Uuid> {
        self.tabs.get(self.active_tab).and_then(|t| t.project_id)
    }

    /// Sync current_project_id from the active tab, recalculate filters, and reset
    /// the active tab's cursor to the first session in the new filtered order.
    pub fn sync_project_filter(&mut self) {
        self.current_project_id = self.active_project_id();
        self.recalculate_filtered_order();
        self.reset_active_tab_selection();
        PersistedState::capture(self).save();
    }

    /// Set all tabs to the same project and sync the global filter.
    /// Called when the user switches projects — all tabs should show the same project.
    pub fn set_all_tabs_project(&mut self, project_id: Option<Uuid>) {
        for tab in &mut self.tabs {
            tab.project_id = project_id;
        }
        self.current_project_id = project_id;
        self.rebind_issue_workspaces(project_id);
        self.recalculate_filtered_order();
        self.reset_selection_to_first();
        PersistedState::capture(self).save();
    }

    /// Get the display name for a tab (project name or "all" for the global tab).
    pub fn tab_display_name(&self, tab_index: usize) -> String {
        self.tabs
            .get(tab_index)
            .and_then(|t| t.project_id)
            .and_then(|pid| self.projects.iter().find(|p| p.id == pid))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "all".to_string())
    }

    /// Switch to an existing "All Projects" tab, or create a new one.
    /// Called from the project picker when the user selects "all projects".
    pub fn open_all_projects_workspace(&mut self) {
        if let Some(idx) = self.tabs.iter().position(|t| t.project_id.is_none()) {
            self.active_tab = idx;
            self.sync_project_filter();
            return;
        }
        let pane_id = self.alloc_pane_id();
        let first_session = self.session_id_at(0);
        let list_pane = Pane::SessionList {
            selected_index: 0,
            selected_session: first_session,
            scroll_offset: 0,
            active_zone: Default::default(),
            taskrabbit_selected_index: 0,
            archive_selected_index: 0,
            jobs_selected_index: 0,
        };
        let tab = crate::types::Tab {
            name: "all".to_string(),
            session_list_state: list_pane.clone(),
            layout: SplitNode::Leaf {
                pane: list_pane,
                id: pane_id,
            },
            focused_pane: pane_id,
            project_id: None,
            detail_split_offset: 0,
            layout_x_offset_adj: 0,
            session_list_width_pct: 0,
            descent_path: Vec::new(),
            bottom_focus_target: crate::types::BottomZone::Off,
            mini_dag_focus: None,
        };
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.sync_project_filter();
    }

    /// Set the current project filter and persist to disk.
    pub fn set_project_filter(&mut self, project_id: Option<Uuid>) {
        self.current_project_id = project_id;
        self.recalculate_filtered_order();
        PersistedState::capture(self).save();
        self.reset_selection_to_first();
    }

    /// Get the current project (if filter is set).
    pub fn current_project(&self) -> Option<&Project> {
        self.current_project_id
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
    }

    /// Update the projects list from daemon response.
    pub fn update_projects(&mut self, projects: Vec<Project>) -> bool {
        let changed = self.projects.len() != projects.len()
            || self
                .projects
                .iter()
                .zip(projects.iter())
                .any(|(a, b)| a.id != b.id || a.updated_at != b.updated_at);
        self.projects = projects;
        if changed {
            self.manager_roster.request_refresh();
        }

        let mut orphaned_indices: Vec<usize> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Some(pid) = tab.project_id {
                if !self.projects.iter().any(|p| p.id == pid) {
                    orphaned_indices.push(i);
                }
            }
        }

        if !orphaned_indices.is_empty() {
            for idx in orphaned_indices.into_iter().rev() {
                if self.tabs.len() > 1 {
                    self.tabs.remove(idx);
                    if self.active_tab >= self.tabs.len() {
                        self.active_tab = self.tabs.len() - 1;
                    }
                } else {
                    self.tabs[0].project_id = None;
                }
            }
            self.sync_project_filter();
            return true;
        }

        changed
    }

    /// Open a new workspace tab for a project.
    pub fn open_project_workspace(&mut self, project_id: Uuid) {
        let pane_id = self.alloc_pane_id();
        let name = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "?".to_string());
        let list_pane = Pane::SessionList {
            selected_index: 0,
            selected_session: None,
            scroll_offset: 0,
            active_zone: Default::default(),
            taskrabbit_selected_index: 0,
            archive_selected_index: 0,
            jobs_selected_index: 0,
        };
        let tab = crate::types::Tab {
            name,
            session_list_state: list_pane.clone(),
            layout: SplitNode::Leaf {
                pane: list_pane,
                id: pane_id,
            },
            focused_pane: pane_id,
            project_id: Some(project_id),
            detail_split_offset: 0,
            layout_x_offset_adj: 0,
            session_list_width_pct: 0,
            descent_path: Vec::new(),
            bottom_focus_target: crate::types::BottomZone::Off,
            mini_dag_focus: None,
        };
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.sync_project_filter();
    }

    /// Update label list from daemon response.
    pub(crate) fn update_labels(&mut self, labels: Vec<rsi_common::types::SessionLabel>) {
        self.labels = labels;
    }
}
