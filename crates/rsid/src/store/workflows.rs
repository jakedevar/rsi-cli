//! Workflow persistence operations.

use super::Store;
use super::row_mappers::{map_workflow_row, workflow_stage_to_str};
use crate::error::{DaemonError, Result};
use rsi_common::types::{Workflow, WorkflowDocument, WorkflowStage};
use rsi_graph::format::WorkflowDefinition;
use rusqlite::params;
use uuid::Uuid;

impl Store {
    /// Insert a new workflow.
    pub fn insert_workflow(&self, workflow: &Workflow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO workflows (id, title, stage, artifact_path, project_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                workflow.id.to_string(),
                workflow.title,
                workflow_stage_to_str(workflow.stage),
                workflow.artifact_path.as_deref(),
                workflow.project_id.map(|id| id.to_string()),
                workflow.created_at.to_rfc3339(),
                workflow.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get a single workflow by ID.
    pub fn get_workflow(&self, id: Uuid) -> Result<Option<Workflow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, stage, artifact_path, project_id, created_at, updated_at
             FROM workflows WHERE id = ?1",
        )?;

        let mut rows = stmt.query_map(params![id.to_string()], map_workflow_row)?;

        match rows.next() {
            Some(Ok(row)) => Ok(Some(row.into_workflow()?)),
            Some(Err(e)) => Err(crate::error::DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Fetch workflow metadata plus the stored definition payload.
    pub fn get_workflow_definition(&self, id: Uuid) -> Result<Option<WorkflowDocument>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, stage, artifact_path, project_id, created_at, updated_at, definition_json
             FROM workflows WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id.to_string()])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };

        let workflow = map_workflow_row(row)?.into_workflow()?;
        let definition_json: Option<String> = row.get(7)?;

        Ok(Some(WorkflowDocument {
            definition: parse_definition_json(&workflow, definition_json.as_deref())?,
            workflow,
        }))
    }

    /// Insert or update workflow metadata and definition in one transaction.
    pub fn upsert_workflow_definition(
        &self,
        document: &WorkflowDocument,
    ) -> Result<WorkflowDocument> {
        let normalized = normalize_workflow_document(document)?;
        let definition_json = serde_json::to_string(&normalized.definition)?;

        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION;")?;

        let result = (|| -> Result<()> {
            self.conn.execute(
                "INSERT INTO workflows (
                    id, title, stage, artifact_path, definition_json, project_id, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    title = excluded.title,
                    stage = excluded.stage,
                    artifact_path = excluded.artifact_path,
                    definition_json = excluded.definition_json,
                    project_id = excluded.project_id,
                    updated_at = excluded.updated_at",
                params![
                    normalized.workflow.id.to_string(),
                    normalized.workflow.title,
                    workflow_stage_to_str(normalized.workflow.stage),
                    normalized.workflow.artifact_path.as_deref(),
                    definition_json,
                    normalized.workflow.project_id.map(|value| value.to_string()),
                    normalized.workflow.created_at.to_rfc3339(),
                    normalized.workflow.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(normalized)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(error)
            }
        }
    }

    /// List all workflows.
    pub fn list_workflows(&self) -> Result<Vec<Workflow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, stage, artifact_path, project_id, created_at, updated_at
             FROM workflows ORDER BY created_at DESC",
        )?;

        let rows = stmt
            .query_map([], map_workflow_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter().map(|row| row.into_workflow()).collect()
    }

    /// List workflows filtered by project_id.
    pub fn list_workflows_by_project(&self, project_id: Option<Uuid>) -> Result<Vec<Workflow>> {
        let (sql, pid_str) = match project_id {
            Some(pid) => (
                "SELECT id, title, stage, artifact_path, project_id, created_at, updated_at
                 FROM workflows
                 WHERE project_id = ?1 OR project_id IS NULL
                 ORDER BY created_at DESC",
                Some(pid.to_string()),
            ),
            None => (
                "SELECT id, title, stage, artifact_path, project_id, created_at, updated_at
                 FROM workflows WHERE project_id IS NULL ORDER BY created_at DESC",
                None,
            ),
        };

        let mut stmt = self.conn.prepare(sql)?;

        let rows = match pid_str {
            Some(pid) => stmt
                .query_map(params![pid], map_workflow_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            None => stmt
                .query_map([], map_workflow_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };

        rows.into_iter().map(|row| row.into_workflow()).collect()
    }

    /// Update a workflow's stage and optionally its artifact_path.
    pub fn update_workflow_stage(
        &self,
        id: Uuid,
        stage: WorkflowStage,
        artifact_path: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        match artifact_path {
            Some(path) => {
                self.conn.execute(
                    "UPDATE workflows SET stage = ?1, artifact_path = ?2, updated_at = ?3 WHERE id = ?4",
                    params![
                        workflow_stage_to_str(stage),
                        path,
                        now,
                        id.to_string(),
                    ],
                )?;
            }
            None => {
                self.conn.execute(
                    "UPDATE workflows SET stage = ?1, updated_at = ?2 WHERE id = ?3",
                    params![workflow_stage_to_str(stage), now, id.to_string(),],
                )?;
            }
        }
        Ok(())
    }

    /// Update a session's workflow_id.
    pub fn update_session_workflow(
        &self,
        session_id: Uuid,
        workflow_id: Option<Uuid>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET workflow_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                workflow_id.map(|id| id.to_string()),
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Delete a workflow row by id (P1.12 §9 cascade support).
    ///
    /// Idempotent: returns `Ok(())` whether or not the row existed beforehand.
    /// Used by the bridge's `delete_workflow_by_source_topology` cascade to
    /// drop the workflow row mirrored from a topology before the topology
    /// itself is deleted (OQ2 ordering: workflow first, topology second).
    pub fn delete_workflow(&self, id: Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM workflows WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }
}

fn normalize_workflow_document(document: &WorkflowDocument) -> Result<WorkflowDocument> {
    let mut workflow = document.workflow.clone();
    let mut definition = match &document.definition {
        serde_json::Value::Null => serde_json::to_value(WorkflowDefinition::new(&workflow.title))?,
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        _ => {
            return Err(DaemonError::Store(
                "workflow definition must be a JSON object".to_string(),
            ));
        }
    };

    let name_from_definition = definition
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let synced_title = name_from_definition.unwrap_or_else(|| workflow.title.trim().to_string());
    if synced_title.is_empty() {
        return Err(DaemonError::Store(
            "workflow title cannot be empty".to_string(),
        ));
    }

    workflow.title = synced_title.clone();

    let object = definition
        .as_object_mut()
        .expect("validated object definition");
    object.insert("name".to_string(), serde_json::Value::String(synced_title));
    object
        .entry("version".to_string())
        .or_insert_with(|| serde_json::Value::String("1.0".to_string()));
    object
        .entry("description".to_string())
        .or_insert_with(|| serde_json::Value::String(String::new()));
    object
        .entry("nodes".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    object
        .entry("edges".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    object
        .entry("metadata".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    Ok(WorkflowDocument {
        workflow,
        definition,
    })
}

fn parse_definition_json(
    workflow: &Workflow,
    definition_json: Option<&str>,
) -> Result<serde_json::Value> {
    match definition_json {
        Some(json) if !json.trim().is_empty() => Ok(serde_json::from_str(json)?),
        _ => Ok(serde_json::to_value(WorkflowDefinition::new(
            &workflow.title,
        ))?),
    }
}
