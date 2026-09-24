//! DB-stored named topology template CRUD operations.
//!
//! Table: `topologies (id, name, definition_json, created_at, updated_at)`.
//! Provisioned by V43 migration; no schema change in this module.

use super::Store;
use super::row_mappers::parse_timestamp;
use crate::error::{DaemonError, Result};
use rsi_common::types::Topology;
use rusqlite::params;
use uuid::Uuid;

/// Intermediate row for reading topologies from the database.
///
/// Mirrors `SessionLabelRow` at `store/labels.rs:14-22`. The serde_json
/// deserialization of `definition_json` lives in `into_topology()` so a
/// malformed row surfaces a clear `DaemonError::Store` rather than a
/// cryptic rusqlite error mid-iteration.
struct TopologyRow {
    id_str: String,
    name: String,
    definition_json: String,
    created_at_str: String,
    updated_at_str: String,
}

impl TopologyRow {
    fn into_topology(self) -> Result<Topology> {
        let id = Uuid::parse_str(&self.id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid topology UUID: {}", e)))?;
        let definition = serde_json::from_str(&self.definition_json)
            .map_err(|e| DaemonError::Store(format!("Invalid topology definition JSON: {}", e)))?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;
        let updated_at = parse_timestamp(&self.updated_at_str).map_err(DaemonError::Store)?;

        Ok(Topology {
            id,
            name: self.name,
            definition,
            created_at,
            updated_at,
        })
    }
}

fn map_topology_row(row: &rusqlite::Row) -> rusqlite::Result<TopologyRow> {
    Ok(TopologyRow {
        id_str: row.get(0)?,
        name: row.get(1)?,
        definition_json: row.get(2)?,
        created_at_str: row.get(3)?,
        updated_at_str: row.get(4)?,
    })
}

impl Store {
    /// Insert a new topology row. Caller is responsible for the name-uniqueness
    /// pre-check; the DB UNIQUE constraint on `name` is the final guard.
    pub fn insert_topology(&self, topology: &Topology) -> Result<()> {
        let definition_json = serde_json::to_string(&topology.definition).map_err(|e| {
            DaemonError::Store(format!("Failed to serialize topology definition: {}", e))
        })?;
        self.conn.execute(
            "INSERT INTO topologies (id, name, definition_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                topology.id.to_string(),
                topology.name,
                definition_json,
                topology.created_at.to_rfc3339(),
                topology.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Update an existing topology row (in-place rewrite of name +
    /// `definition_json` + `updated_at`).
    pub fn update_topology(&self, topology: &Topology) -> Result<()> {
        let definition_json = serde_json::to_string(&topology.definition).map_err(|e| {
            DaemonError::Store(format!("Failed to serialize topology definition: {}", e))
        })?;
        self.conn.execute(
            "UPDATE topologies SET name = ?1, definition_json = ?2, updated_at = ?3
             WHERE id = ?4",
            params![
                topology.name,
                definition_json,
                topology.updated_at.to_rfc3339(),
                topology.id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Delete a topology row by id. Caller (SessionManager) is responsible
    /// for the in-use rejection check via `is_topology_referenced`.
    pub fn delete_topology(&self, id: Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM topologies WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    /// Fetch a single topology by id. Returns `Ok(None)` if no row matches.
    pub fn get_topology(&self, id: Uuid) -> Result<Option<Topology>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, definition_json, created_at, updated_at
             FROM topologies WHERE id = ?1",
        )?;

        let mut rows = stmt.query_map(params![id.to_string()], map_topology_row)?;

        match rows.next() {
            Some(Ok(row)) => Ok(Some(row.into_topology()?)),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Load all topologies ordered by name. When `name_prefix` is `Some`,
    /// filters to rows where `name LIKE 'prefix%'` (uses
    /// `idx_topologies_name` for the prefix scan).
    pub fn load_topologies(&self, name_prefix: Option<&str>) -> Result<Vec<Topology>> {
        let rows = if let Some(prefix) = name_prefix {
            let mut stmt = self.conn.prepare(
                "SELECT id, name, definition_json, created_at, updated_at
                 FROM topologies
                 WHERE name LIKE ?1 || '%'
                 ORDER BY name ASC",
            )?;
            stmt.query_map(params![prefix], map_topology_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, name, definition_json, created_at, updated_at
                 FROM topologies
                 ORDER BY name ASC",
            )?;
            stmt.query_map([], map_topology_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        rows.into_iter().map(TopologyRow::into_topology).collect()
    }

    /// Returns true if a topology row with the given id exists.
    pub fn topology_exists(&self, id: Uuid) -> Result<bool> {
        use rusqlite::OptionalExtension;
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM topologies WHERE id = ?1 LIMIT 1",
                [id.to_string()],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(exists)
    }

    /// Check whether any Epic session row still references this topology
    /// via its primary `workflow_id` pointer. Returns the offending Epic's
    /// UUID if found (for richer error messaging). Does NOT consult
    /// `workflow_id_override` per Decision 1 (P1.4 plan).
    pub fn is_topology_referenced(&self, id: Uuid) -> Result<Option<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions
             WHERE workflow_id = ?1 AND session_kind = 'Epic'
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![id.to_string()], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;

        match rows.next() {
            Some(Ok(id_str)) => Uuid::parse_str(&id_str)
                .map(Some)
                .map_err(|e| DaemonError::Store(format!("Invalid session UUID: {}", e))),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Check whether a topology with the given name exists, excluding the
    /// optional `exclude_id` (for update-rename uniqueness checks).
    /// Returns the existing row's UUID if a conflict is found.
    pub fn topology_name_exists(
        &self,
        name: &str,
        exclude_id: Option<Uuid>,
    ) -> Result<Option<Uuid>> {
        // When `exclude_id` is None, use the empty string sentinel: no real
        // UUID stringifies to "", so the `id != ?2` predicate matches every
        // row. This keeps a single prepared statement for both code paths.
        let exclude_str = exclude_id.map(|id| id.to_string()).unwrap_or_default();
        let mut stmt = self.conn.prepare(
            "SELECT id FROM topologies
             WHERE name = ?1 AND id != ?2
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![name, exclude_str], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;

        match rows.next() {
            Some(Ok(id_str)) => Uuid::parse_str(&id_str)
                .map(Some)
                .map_err(|e| DaemonError::Store(format!("Invalid topology UUID: {}", e))),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }
}
