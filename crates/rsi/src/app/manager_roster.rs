//! Daemon-resolved manager principals for the session navigator.

use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use rsi_common::harness_manager::HarnessManagerConfigV1;

pub(super) type RefreshResults = Vec<(Uuid, Result<Option<HarnessManagerConfigV1>, String>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerTier {
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerRosterEntry {
    pub session_id: Uuid,
    pub tier: ManagerTier,
}

#[derive(Debug, Default)]
pub struct ManagerRoster {
    pub by_project: HashMap<Uuid, ManagerRosterEntry>,
    pending: bool,
}

impl ManagerRoster {
    pub const fn request_refresh(&mut self) {
        self.pending = true;
    }

    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }

    pub fn member_ids(&self) -> HashSet<Uuid> {
        self.by_project
            .values()
            .map(|entry| entry.session_id)
            .collect()
    }

    pub fn contains(&self, session_id: Uuid) -> bool {
        self.by_project
            .values()
            .any(|entry| entry.session_id == session_id)
    }

    pub fn retain_projects(&mut self, projects: &HashSet<Uuid>) -> bool {
        let before = self.by_project.len();
        self.by_project
            .retain(|project_id, _| projects.contains(project_id));
        self.by_project.len() != before
    }

    /// An RPC error leaves the last daemon-verified entry intact.
    pub fn apply_result(
        &mut self,
        project_id: Uuid,
        result: Result<Option<HarnessManagerConfigV1>, String>,
    ) -> Result<bool, String> {
        let config = result?;
        let next = config.and_then(|config| {
            config
                .current_session_id
                .map(|session_id| ManagerRosterEntry {
                    session_id,
                    tier: ManagerTier::Project,
                })
        });
        let previous = match next {
            Some(entry) => self.by_project.insert(project_id, entry),
            None => self.by_project.remove(&project_id),
        };
        Ok(previous != next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_removes_entry_and_error_preserves_prior() {
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let mut roster = ManagerRoster::default();
        roster.by_project.insert(
            project,
            ManagerRosterEntry {
                session_id: session,
                tier: ManagerTier::Project,
            },
        );
        assert_eq!(
            roster.apply_result(project, Err("offline".into())),
            Err("offline".into())
        );
        assert!(roster.contains(session));
        assert_eq!(roster.apply_result(project, Ok(None)), Ok(true));
        assert_eq!(roster.by_project.len(), 0);
    }

    #[test]
    fn config_without_current_principal_removes_entry() {
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let mut roster = ManagerRoster::default();
        roster.by_project.insert(
            project,
            ManagerRosterEntry {
                session_id: session,
                tier: ManagerTier::Project,
            },
        );
        let config = HarnessManagerConfigV1 {
            project_id: project,
            manager_session_id: session,
            current_session_id: None,
            epic_ids: Vec::new(),
            scope_mode: rsi_common::harness_manager::HarnessManagerScopeModeV1::default(),
            selected_epic_ids: None,
            group_ids: Vec::new(),
            row_version: 1,
            updated_at: chrono::Utc::now(),
        };
        assert_eq!(roster.apply_result(project, Ok(Some(config))), Ok(true));
        assert_eq!(roster.by_project.len(), 0);
    }

    #[test]
    fn refresh_requests_coalesce() {
        let mut roster = ManagerRoster::default();
        roster.request_refresh();
        roster.request_refresh();
        assert!(roster.take_refresh());
        assert!(!roster.take_refresh());
    }
}
