//! Session label persistence operations.
//!
//! Note: SQL table name `session_groups` and column `group_id` are preserved
//! for schema compatibility. Only the Rust types/functions are renamed.

use super::Store;
use super::row_mappers::parse_timestamp;
use crate::error::{DaemonError, Result};
use rsi_common::types::SessionLabel;
use rusqlite::params;
use uuid::Uuid;

/// Intermediate row for reading session labels from the database.
struct SessionLabelRow {
    id_str: String,
    name: String,
    description: Option<String>,
    project_id_str: Option<String>,
    color: String,
    created_at_str: String,
    updated_at_str: String,
}

impl SessionLabelRow {
    fn into_session_label(self) -> Result<SessionLabel> {
        let id = Uuid::parse_str(&self.id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid label UUID: {}", e)))?;
        let project_id = self
            .project_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid label project UUID: {}", e)))?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;
        let updated_at = parse_timestamp(&self.updated_at_str).map_err(DaemonError::Store)?;

        Ok(SessionLabel {
            id,
            name: self.name,
            description: self.description,
            project_id,
            color: self.color,
            created_at,
            updated_at,
        })
    }
}

fn map_label_row(row: &rusqlite::Row) -> rusqlite::Result<SessionLabelRow> {
    Ok(SessionLabelRow {
        id_str: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        project_id_str: row.get(3)?,
        color: row.get(4)?,
        created_at_str: row.get(5)?,
        updated_at_str: row.get(6)?,
    })
}

impl Store {
    /// Insert a new session label.
    pub fn insert_label(&self, label: &SessionLabel) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_groups (id, name, description, project_id, color, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                label.id.to_string(),
                label.name,
                label.description,
                label.project_id.map(|id| id.to_string()),
                label.color,
                label.created_at.to_rfc3339(),
                label.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Update an existing session label.
    pub fn update_label(&self, label: &SessionLabel) -> Result<()> {
        self.conn.execute(
            "UPDATE session_groups SET name = ?1, description = ?2, project_id = ?3, color = ?4, updated_at = ?5
             WHERE id = ?6",
            params![
                label.name,
                label.description,
                label.project_id.map(|id| id.to_string()),
                label.color,
                chrono::Utc::now().to_rfc3339(),
                label.id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Delete a session label by ID.
    /// Clears group_id from sessions referencing this label, then deletes the label.
    pub fn delete_label(&self, id: Uuid) -> Result<()> {
        let id_str = id.to_string();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE sessions SET group_id = NULL WHERE group_id = ?1",
            params![&id_str],
        )?;
        tx.execute("DELETE FROM session_groups WHERE id = ?1", params![&id_str])?;
        tx.commit()?;
        Ok(())
    }

    /// Get a session label by ID.
    pub fn get_label(&self, id: Uuid) -> Result<Option<SessionLabel>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, description, project_id, color, created_at, updated_at
             FROM session_groups WHERE id = ?1",
        )?;

        let mut rows = stmt.query_map(params![id.to_string()], map_label_row)?;

        match rows.next() {
            Some(Ok(row)) => Ok(Some(row.into_session_label()?)),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Load all session labels ordered by name.
    pub fn load_labels(&self) -> Result<Vec<SessionLabel>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, description, project_id, color, created_at, updated_at
             FROM session_groups ORDER BY name ASC",
        )?;

        let rows = stmt
            .query_map([], map_label_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|row| row.into_session_label())
            .collect()
    }
}
