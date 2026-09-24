//! Permission rules management.

use crate::error::{DaemonError, Result};
use crate::store::Store;
use chrono::Utc;
use rsi_common::types::{PermissionLevel, PermissionRule, PermissionScope};
use rusqlite::params;
use uuid::Uuid;

impl Store {
    /// Insert a new permission rule.
    pub fn insert_permission_rule(
        &self,
        tool_pattern: &str,
        level: PermissionLevel,
        scope: PermissionScope,
        scope_id: Option<Uuid>,
        priority: i32,
    ) -> Result<i64> {
        let action = match level {
            PermissionLevel::Allow => "Allow",
            PermissionLevel::Ask => "Ask",
            PermissionLevel::Deny => "Deny",
            _ => "Ask",
        };

        let scope_str = match scope {
            PermissionScope::Global => "global",
            PermissionScope::Project => "project",
            _ => "global",
        };

        let scope_id_str = scope_id.map(|id| id.to_string());
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);

        self.conn.execute(
            "INSERT INTO permission_rules (tool_pattern, action, scope, scope_id, priority, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                tool_pattern,
                action,
                scope_str,
                scope_id_str,
                priority,
                now.clone(),
                now,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// List permission rules, optionally filtered by scope.
    pub fn list_permission_rules(
        &self,
        scope: Option<&str>,
        scope_id: Option<Uuid>,
    ) -> Result<Vec<PermissionRule>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, tool_pattern, action, scope, scope_id, priority, created_at
             FROM permission_rules
             ORDER BY priority ASC, id ASC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i32>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;

        let mut result = Vec::new();
        for row_result in rows {
            let (id, tool_pattern, action, scope_str, scope_id_str, priority, created_at_str) =
                row_result?;

            // Filter by scope if provided
            if let Some(scope_filter) = scope {
                if scope_str != scope_filter {
                    continue;
                }
                if let Some(scope_id_filter) = scope_id {
                    if let Some(ref sid_str) = scope_id_str {
                        if sid_str != &scope_id_filter.to_string() {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
            }

            let level = match action.as_str() {
                "Allow" => PermissionLevel::Allow,
                "Deny" => PermissionLevel::Deny,
                _ => PermissionLevel::Ask,
            };

            let scope_enum = match scope_str.as_str() {
                "project" => PermissionScope::Project,
                _ => PermissionScope::Global,
            };

            let scope_id_uuid = scope_id_str.and_then(|id_str| Uuid::parse_str(&id_str).ok());
            let created_at = crate::store::parse_timestamp(&created_at_str)
                .map_err(|e| DaemonError::Store(e))?;

            result.push(PermissionRule {
                id,
                tool_pattern,
                level,
                scope: scope_enum,
                scope_id: scope_id_uuid,
                priority,
                created_at,
            });
        }

        Ok(result)
    }

    /// Delete a permission rule by ID.
    pub fn delete_permission_rule(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM permission_rules WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Update a permission rule's action and priority.
    pub fn update_permission_rule(&self, id: i64, action: &str, priority: i32) -> Result<()> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        self.conn.execute(
            "UPDATE permission_rules SET action = ?1, priority = ?2, updated_at = ?3 WHERE id = ?4",
            params![action, priority, now, id],
        )?;
        Ok(())
    }
}
