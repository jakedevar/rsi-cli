//! Project persistence operations.

use super::Store;
use super::row_mappers::ProjectRow;
use crate::error::{DaemonError, Result};
use rsi_common::types::Project;
use rusqlite::params;
use uuid::Uuid;

impl Store {
    /// Insert a new project.
    pub fn insert_project(&self, project: &Project) -> Result<()> {
        let context_files_json = project
            .context_files
            .as_ref()
            .map(|f| serde_json::to_string(f).unwrap_or_default());
        self.conn.execute(
            "INSERT INTO projects (id, name, path, description, color, context_files, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                project.id.to_string(),
                project.name,
                project
                    .path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string()),
                project.description,
                project.color,
                context_files_json,
                project.created_at.to_rfc3339(),
                project.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Update an existing project.
    ///
    /// # Errors
    /// `home_project_repath_refused` when `project` is the built-in home
    /// project with a path other than its stored one (hub #3315), or a
    /// persistence error.
    pub fn update_project(&self, project: &Project) -> Result<()> {
        if Project::is_home_id(project.id)
            && let Some(stored) = self.get_project(project.id)?
            && stored.path != project.path
        {
            return Err(DaemonError::InvalidParam(
                Project::HOME_REPATH_REFUSED.into(),
            ));
        }
        let context_files_json = project
            .context_files
            .as_ref()
            .map(|f| serde_json::to_string(f).unwrap_or_default());
        self.conn.execute(
            "UPDATE projects SET name = ?1, path = ?2, description = ?3, color = ?4, context_files = ?5, updated_at = ?6
             WHERE id = ?7",
            params![
                project.name,
                project.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                project.description,
                project.color,
                context_files_json,
                chrono::Utc::now().to_rfc3339(),
                project.id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Delete a project by ID.
    /// Clears project_id from sessions referencing this project, then deletes the project.
    ///
    /// # Errors
    /// `home_project_delete_refused` for the built-in home project (hub
    /// #3315), the manager-authority refusal or a persistence error.
    pub fn delete_project(&self, id: Uuid) -> Result<()> {
        if Project::is_home_id(id) {
            return Err(DaemonError::InvalidParam(
                Project::HOME_DELETE_REFUSED.into(),
            ));
        }
        let id_str = id.to_string();
        let tx = self.conn.unchecked_transaction()?;
        // V111 seeds every project at 1, including projects with older manager
        // history. Only remove the synthetic fence with no retained appointment
        // or rotation; check rotations before detaching their session references.
        // Other project/epoch foreign keys still restrict the final transaction.
        let removed_epoch = tx.execute(
            "DELETE FROM manager_authority_epochs
             WHERE project_id = ?1 AND typeof(epoch) = 'integer' AND epoch = 1
               AND NOT EXISTS (
                   SELECT 1 FROM harness_manager_scopes WHERE project_id = ?1
               )
               AND NOT EXISTS (
                   SELECT 1 FROM harness_manager_rotation_edges edge
                   JOIN sessions session
                     ON session.id = edge.predecessor_session_id
                     OR session.id = edge.successor_session_id
                   WHERE session.project_id = ?1
               )",
            params![&id_str],
        )?;
        if removed_epoch != 1
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
                params![&id_str],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(DaemonError::InvalidParam(
                "project deletion requires pristine manager authority state".into(),
            ));
        }
        tx.execute(
            "UPDATE sessions SET project_id = NULL WHERE project_id = ?1",
            params![&id_str],
        )?;
        tx.execute("DELETE FROM projects WHERE id = ?1", params![&id_str])?;
        tx.commit()?;
        Ok(())
    }

    /// Hub #3315: register the built-in home project ([`Project::HOME_ID`])
    /// at `home`, the daemon's `$HOME`. Idempotent: a store that already has
    /// it keeps that one row (its path follows `home` if the daemon's home
    /// moved; this is the only writer of that path). The name is
    /// [`Project::HOME_NAME`], or the first free `"<name> (n)"` when an
    /// operator project already uses it.
    ///
    /// # Errors
    /// A persistence error.
    pub fn ensure_home_project(&self, home: &std::path::Path) -> Result<Project> {
        if let Some(existing) = self.get_project(Project::HOME_ID)? {
            if existing.path.as_deref() == Some(home) {
                return Ok(existing);
            }
            let now = chrono::Utc::now();
            self.conn.execute(
                "UPDATE projects SET path = ?1, updated_at = ?2 WHERE id = ?3",
                params![
                    home.to_string_lossy().to_string(),
                    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    Project::HOME_ID.to_string(),
                ],
            )?;
            return self
                .get_project(Project::HOME_ID)?
                .ok_or_else(|| DaemonError::Store("home project vanished".into()));
        }
        let mut name = Project::HOME_NAME.to_string();
        let mut suffix = 2;
        while self.get_project_by_name(&name)?.is_some() {
            name = format!("{} ({suffix})", Project::HOME_NAME);
            suffix += 1;
        }
        let now = chrono::Utc::now();
        let project = Project {
            id: Project::HOME_ID,
            name,
            path: Some(home.to_path_buf()),
            description: Some(
                "Built-in RSI home: global and portfolio manager seats run here, without a sandbox."
                    .into(),
            ),
            color: Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        self.insert_project(&project)?;
        Ok(project)
    }

    /// Get a project by ID.
    pub fn get_project(&self, id: Uuid) -> Result<Option<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, path, description, color, context_files, created_at, updated_at
             FROM projects WHERE id = ?1",
        )?;

        let mut rows = stmt.query_map(params![id.to_string()], |row| {
            Ok(ProjectRow {
                id_str: row.get(0)?,
                name: row.get(1)?,
                path_str: row.get(2)?,
                description: row.get(3)?,
                color: row.get(4)?,
                context_files_json: row.get(5)?,
                created_at_str: row.get(6)?,
                updated_at_str: row.get(7)?,
            })
        })?;

        match rows.next() {
            Some(Ok(row)) => Ok(Some(row.into_project()?)),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Get a project by name (case-sensitive exact match).
    pub fn get_project_by_name(&self, name: &str) -> Result<Option<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, path, description, color, context_files, created_at, updated_at
             FROM projects WHERE name = ?1",
        )?;

        let mut rows = stmt.query_map(params![name], |row| {
            Ok(ProjectRow {
                id_str: row.get(0)?,
                name: row.get(1)?,
                path_str: row.get(2)?,
                description: row.get(3)?,
                color: row.get(4)?,
                context_files_json: row.get(5)?,
                created_at_str: row.get(6)?,
                updated_at_str: row.get(7)?,
            })
        })?;

        match rows.next() {
            Some(Ok(row)) => Ok(Some(row.into_project()?)),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Load all projects ordered by name.
    pub fn load_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, path, description, color, context_files, created_at, updated_at
             FROM projects ORDER BY name ASC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                Ok(ProjectRow {
                    id_str: row.get(0)?,
                    name: row.get(1)?,
                    path_str: row.get(2)?,
                    description: row.get(3)?,
                    color: row.get(4)?,
                    context_files_json: row.get(5)?,
                    created_at_str: row.get(6)?,
                    updated_at_str: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter().map(|row| row.into_project()).collect()
    }

    /// Find the best-matching project for a working directory.
    /// Returns the project with the longest path prefix match.
    pub fn find_project_for_path(&self, working_dir: &std::path::Path) -> Result<Option<Project>> {
        let projects = self.load_projects()?;
        let working_dir_str = working_dir.to_string_lossy();

        let mut best_match: Option<Project> = None;
        let mut longest_prefix_len = 0;

        // Hub #3315: the home project is never matched by path, or it would
        // claim every unregistered directory under `$HOME`.
        for project in projects.into_iter().filter(|p| !Project::is_home_id(p.id)) {
            if let Some(ref project_path) = project.path {
                let project_path_str = project_path.to_string_lossy();
                if working_dir_str.starts_with(project_path_str.as_ref())
                    && project_path_str.len() > longest_prefix_len
                {
                    longest_prefix_len = project_path_str.len();
                    best_match = Some(project);
                }
            }
        }

        Ok(best_match)
    }
}
