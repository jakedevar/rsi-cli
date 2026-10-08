//! Agent-created projects (#1626 slice 1): authority resolution and the
//! guarded writes behind `AgentCreateProject` and `AgentUpdateProject`.
//!
//! Authority mirrors the catalog (`VerbRights::project_admin`): an
//! Execute-mode, unpaused project manager acts on its own project; an
//! Execute-mode, unpaused portfolio seat acts on its grant's coverage. Every
//! check and write runs under one store lock hold, so a revoked seat cannot
//! race a write. Refusals use stable codes from `rsi_common::agent_projects`;
//! a project outside the caller's coverage and a project that does not exist
//! are the same refusal.

use std::path::{Path, PathBuf};

use rsi_common::agent_projects::{
    PROJECT_HARNESS_PROTECTED, PROJECT_HAS_LIVE_SESSIONS, PROJECT_NAME_TAKEN,
    PROJECT_NOT_AUTHORIZED, PROJECT_NOT_IN_SCOPE, PROJECT_PATH_TAKEN,
};
use rsi_common::harness_manager_v2::ManagerOperatingModeV2;
use rsi_common::types::{Project, SessionStatus};
use uuid::Uuid;

use super::Store;
use crate::error::{DaemonError, Result};

fn refuse(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

/// The projects an agent may administer: the union of the coverage of every
/// seat it holds that permits it (its own project as an Execute-mode,
/// unpaused project manager; its grant's projects as an Execute-mode,
/// unpaused portfolio seat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProjectScope {
    projects: Vec<Uuid>,
}

impl AgentProjectScope {
    #[must_use]
    pub fn covers(&self, project: Uuid) -> bool {
        self.projects.contains(&project)
    }
}

/// Field changes for [`Store::agent_edit_project`].
#[derive(Debug, Clone, Default)]
pub struct AgentProjectEdit {
    pub name: Option<String>,
    /// Canonical absolute directory (validated by the caller).
    pub path: Option<PathBuf>,
    pub description: Option<String>,
    pub color: Option<String>,
}

/// Best-effort canonical form of a stored project path, for comparison.
fn comparable(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `path` equals, contains or sits inside the harness root.
fn overlaps_harness_root(path: &Path, harness_root: Option<&Path>) -> bool {
    harness_root.is_some_and(|root| path.starts_with(root) || root.starts_with(path))
}

impl Store {
    /// Resolve what `caller` may administer. Each seat is evaluated
    /// independently: a seat that does not permit it (Status mode, paused,
    /// revoked) contributes nothing and never blocks another seat. The
    /// catalog's `project_admin` right is this function succeeding, so the
    /// catalog and the daemon cannot disagree.
    ///
    /// # Errors
    /// `project_not_authorized` when no seat the caller holds permits it.
    pub fn agent_project_scope(&self, caller: Uuid) -> Result<AgentProjectScope> {
        let mut projects = Vec::new();
        let mut authorized = false;
        if let Some(grant) = self.portfolio_seat_grant(caller)?
            && grant.project_policy.mode == ManagerOperatingModeV2::Execute
            && !grant.project_policy.paused
        {
            authorized = true;
            projects.extend(grant.project_ids);
        }
        if let Some(own) = self.project_manager_scope(caller)? {
            authorized = true;
            projects.push(own);
        }
        if authorized {
            Ok(AgentProjectScope { projects })
        } else {
            Err(refuse(PROJECT_NOT_AUTHORIZED))
        }
    }

    /// Whether `caller` may administer projects (the catalog's right).
    ///
    /// # Errors
    /// A persistence error.
    pub fn agent_project_admin(&self, caller: Uuid) -> Result<bool> {
        match self.agent_project_scope(caller) {
            Ok(_) => Ok(true),
            Err(DaemonError::InvalidParam(code)) if code == PROJECT_NOT_AUTHORIZED => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The project `caller` manages in Execute mode, unpaused, if any.
    fn project_manager_scope(&self, caller: Uuid) -> Result<Option<Uuid>> {
        let Some(session) = self
            .get_session(caller)?
            .filter(|s| !matches!(s.status, SessionStatus::Archived | SessionStatus::Deleted))
        else {
            return Ok(None);
        };
        let Some(project) = session.project_id else {
            return Ok(None);
        };
        let Some(config) = self
            .get_harness_manager(project)?
            .filter(|config| config.current_session_id == Some(caller))
        else {
            return Ok(None);
        };
        let active = self.get_harness_manager_policy(project)?.is_some_and(|g| {
            !g.revoked
                && g.manager_session_id == config.manager_session_id
                && g.scope_version == config.row_version
                && g.policy.mode == ManagerOperatingModeV2::Execute
                && !g.policy.paused
        });
        Ok(active.then_some(project))
    }

    fn project_using_path(
        &self,
        canonical: &Path,
        except: Option<Uuid>,
    ) -> Result<Option<Project>> {
        Ok(self.load_projects()?.into_iter().find(|existing| {
            Some(existing.id) != except
                && existing
                    .path
                    .as_deref()
                    .is_some_and(|path| comparable(path) == canonical)
        }))
    }

    /// Register `project` for `caller` (its `path` is the canonical
    /// directory). Returns the stored project and whether it already existed
    /// with the same name and directory (a replay). A replay is answered only
    /// for a project inside the caller's coverage; otherwise it is
    /// `project_name_taken`, which carries no project fields.
    /// `harness_root` is the canonical harness root, when the daemon has one.
    ///
    /// # Errors
    /// `project_not_authorized`, `project_harness_protected`,
    /// `project_name_taken`, `project_path_taken` or a persistence error.
    pub fn agent_register_project(
        &self,
        caller: Uuid,
        project: &Project,
        harness_root: Option<&Path>,
    ) -> Result<(Project, bool)> {
        let scope = self.agent_project_scope(caller)?;
        let canonical = project
            .path
            .as_deref()
            .ok_or_else(|| refuse(rsi_common::agent_projects::PROJECT_PATH_INVALID))?;
        if overlaps_harness_root(canonical, harness_root) {
            return Err(refuse(PROJECT_HARNESS_PROTECTED));
        }
        let tx = self.conn.unchecked_transaction()?;
        let by_name = self
            .load_projects()?
            .into_iter()
            .find(|existing| existing.name == project.name);
        let by_path = self.project_using_path(canonical, None)?;
        match (by_name, by_path) {
            // The same name and directory is a replay (a lost response, a
            // retry): return it, but only inside the caller's coverage, so a
            // guessed name and path never discloses another project.
            (Some(named), Some(pathed)) if named.id == pathed.id && scope.covers(named.id) => {
                return Ok((named, true));
            }
            (Some(_), _) => return Err(refuse(PROJECT_NAME_TAKEN)),
            (None, Some(_)) => return Err(refuse(PROJECT_PATH_TAKEN)),
            (None, None) => {}
        }
        self.insert_project(project)?;
        tx.commit()?;
        Ok((project.clone(), false))
    }

    /// Apply `edit` to `project_id` for `caller`.
    ///
    /// # Errors
    /// `project_not_authorized`, `project_not_in_scope`, `project_name_taken`,
    /// `project_path_taken`, `project_harness_protected`,
    /// `project_has_live_sessions` or a persistence error. The harness
    /// project's path never changes here, and no path may overlap
    /// `harness_root`.
    pub fn agent_edit_project(
        &self,
        caller: Uuid,
        project_id: Uuid,
        edit: AgentProjectEdit,
        harness_root: Option<&Path>,
    ) -> Result<Project> {
        let scope = self.agent_project_scope(caller)?;
        if !scope.covers(project_id) {
            return Err(refuse(PROJECT_NOT_IN_SCOPE));
        }
        let tx = self.conn.unchecked_transaction()?;
        let Some(existing) = self.get_project(project_id)? else {
            return Err(refuse(PROJECT_NOT_IN_SCOPE));
        };
        if let Some(name) = &edit.name
            && *name != existing.name
            && self
                .load_projects()?
                .iter()
                .any(|other| other.id != project_id && other.name == *name)
        {
            return Err(refuse(PROJECT_NAME_TAKEN));
        }
        if let Some(path) = &edit.path
            && existing.path.as_deref().map(comparable).as_deref() != Some(path.as_path())
        {
            let is_harness = harness_root.is_some_and(|root| {
                existing.path.as_deref().map(comparable).as_deref() == Some(root)
            });
            if is_harness || overlaps_harness_root(path, harness_root) {
                return Err(refuse(PROJECT_HARNESS_PROTECTED));
            }
            if self.project_using_path(path, Some(project_id))?.is_some() {
                return Err(refuse(PROJECT_PATH_TAKEN));
            }
            let live: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE project_id=?1
                   AND status IN ('Starting','Running','WaitingApproval')",
                [project_id.to_string()],
                |row| row.get(0),
            )?;
            if live > 0 {
                return Err(refuse(PROJECT_HAS_LIVE_SESSIONS));
            }
        }
        let updated = Project {
            id: existing.id,
            name: edit.name.unwrap_or(existing.name),
            path: edit.path.or(existing.path),
            description: edit.description.or(existing.description),
            color: edit.color.unwrap_or(existing.color),
            context_files: existing.context_files,
            created_at: existing.created_at,
            updated_at: chrono::Utc::now(),
        };
        self.update_project(&updated)?;
        tx.commit()?;
        Ok(updated)
    }
}
