//! Approval persistence operations.

use super::Store;
use super::row_mappers::{approval_status_to_str, parse_timestamp, str_to_approval_status};
use crate::error::{DaemonError, Result};
use rsi_common::types::{Approval, ApprovalStatus};
use rusqlite::params;
use uuid::Uuid;

impl Store {
    /// Insert a pending approval.
    pub fn insert_approval(&self, approval: &Approval) -> Result<()> {
        let tool_input_json = serde_json::to_string(&approval.tool_input).unwrap_or_default();

        self.conn.execute(
            "INSERT INTO approvals (id, session_id, tool_name, tool_input, status, created_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                approval.id.to_string(),
                approval.session_id.to_string(),
                approval.tool_name,
                tool_input_json,
                approval_status_to_str(approval.status),
                approval.created_at.to_rfc3339(),
                approval.resolved_at.map(|dt| dt.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    /// Update approval status (approve/deny).
    pub fn update_approval_status(&self, id: Uuid, status: ApprovalStatus) -> Result<()> {
        let resolved_at = if status != ApprovalStatus::Pending {
            Some(chrono::Utc::now().to_rfc3339())
        } else {
            None
        };

        self.conn.execute(
            "UPDATE approvals SET status = ?1, resolved_at = ?2 WHERE id = ?3",
            params![approval_status_to_str(status), resolved_at, id.to_string(),],
        )?;
        Ok(())
    }

    /// Get pending approvals for a session.
    pub fn get_pending_approvals(&self, session_id: Uuid) -> Result<Vec<Approval>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, tool_name, tool_input, status, created_at, resolved_at
             FROM approvals WHERE session_id = ?1 AND status = 'Pending'
             AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p WHERE p.approval_id=approvals.id AND (p.closure_state='closed' OR p.state='superseded'))",
        )?;

        let rows = stmt
            .query_map(params![session_id.to_string()], |row| {
                let id_str: String = row.get(0)?;
                let session_id_str: String = row.get(1)?;
                let tool_name: String = row.get(2)?;
                let tool_input_str: String = row.get(3)?;
                let status_str: String = row.get(4)?;
                let created_at_str: String = row.get(5)?;
                let resolved_at_str: Option<String> = row.get(6)?;

                Ok((
                    id_str,
                    session_id_str,
                    tool_name,
                    tool_input_str,
                    status_str,
                    created_at_str,
                    resolved_at_str,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(
                    id_str,
                    session_id_str,
                    tool_name,
                    tool_input_str,
                    status_str,
                    created_at_str,
                    resolved_at_str,
                )| {
                    let id = Uuid::parse_str(&id_str)
                        .map_err(|e| DaemonError::Store(format!("Invalid UUID: {}", e)))?;
                    let session_id = Uuid::parse_str(&session_id_str)
                        .map_err(|e| DaemonError::Store(format!("Invalid UUID: {}", e)))?;
                    let tool_input: serde_json::Value = serde_json::from_str(&tool_input_str)
                        .map_err(|e| {
                            DaemonError::Store(format!("Invalid tool_input JSON: {}", e))
                        })?;
                    let status = str_to_approval_status(&status_str)?;
                    let created_at =
                        parse_timestamp(&created_at_str).map_err(DaemonError::Store)?;
                    let resolved_at = resolved_at_str
                        .as_deref()
                        .map(|s| parse_timestamp(s).map_err(DaemonError::Store))
                        .transpose()?;

                    Ok(Approval {
                        id,
                        session_id,
                        tool_name,
                        tool_input,
                        status,
                        created_at,
                        resolved_at,
                    })
                },
            )
            .collect()
    }
}
