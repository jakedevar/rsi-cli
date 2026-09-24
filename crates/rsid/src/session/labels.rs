//! Session label CRUD operations for SessionManager.

use crate::error::{DaemonError, Result};
use rsi_common::types::SessionLabel;
use uuid::Uuid;

use super::SessionManager;

impl SessionManager {
    /// Create a new session label.
    pub async fn create_label(
        &self,
        name: String,
        description: Option<String>,
        project_id: Option<Uuid>,
        color: Option<String>,
    ) -> Result<SessionLabel> {
        let label = SessionLabel {
            id: Uuid::new_v4(),
            name,
            description,
            project_id,
            color: color.unwrap_or_else(|| SessionLabel::DEFAULT_COLOR.to_string()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        self.persistence.insert_label(label.clone()).await?;
        Ok(label)
    }

    /// Update an existing session label.
    pub async fn update_label(
        &self,
        id: Uuid,
        name: Option<String>,
        description: Option<String>,
        color: Option<String>,
    ) -> Result<SessionLabel> {
        let store = self.store.clone();
        let existing = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_label(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??
        .ok_or_else(|| DaemonError::Store(format!("Label not found: {}", id)))?;

        let updated = SessionLabel {
            id: existing.id,
            name: name.unwrap_or(existing.name),
            description: description.or(existing.description),
            project_id: existing.project_id,
            color: color.unwrap_or(existing.color),
            created_at: existing.created_at,
            updated_at: chrono::Utc::now(),
        };
        self.persistence.update_label(updated.clone()).await?;
        Ok(updated)
    }

    /// Delete a session label (clears group_id from referencing sessions).
    pub async fn delete_label(&self, id: Uuid) -> Result<()> {
        self.persistence.delete_label(id).await?;
        // Clear group_id from in-memory active/completed sessions
        {
            let mut active_guard = self.active.write().await;
            for tracked in active_guard.values_mut() {
                if tracked.session.group_id == Some(id) {
                    tracked.session.group_id = None;
                }
            }
        }
        {
            let mut completed_guard = self.completed.write().await;
            for completed in completed_guard.values_mut() {
                if completed.session.group_id == Some(id) {
                    completed.session.group_id = None;
                }
            }
        }
        Ok(())
    }

    /// List all session labels.
    pub async fn list_labels(&self) -> Result<Vec<SessionLabel>> {
        let store = self.store.clone();
        let labels = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_labels()
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;
        Ok(labels)
    }

    /// Get a single session label by ID.
    pub async fn get_label(&self, id: Uuid) -> Result<Option<SessionLabel>> {
        let store = self.store.clone();
        let label = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_label(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;
        Ok(label)
    }

    /// Update a session's label assignment.
    pub async fn update_session_label(
        &self,
        session_id: Uuid,
        group_id: Option<Uuid>,
    ) -> Result<()> {
        self.persistence
            .update_session_label(session_id, group_id)
            .await?;
        // Update in-memory state
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.group_id = group_id;
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.group_id = group_id;
        }
        self.event_bus
            .publish(crate::bus::DaemonEvent::SessionMetadataChanged {
                session_id,
                model: None,
                pinned_at: None,
                project_id: None,
                parent_id: None,
                lead_session_id: None,
                testing_needed_at: None,
                rotation_disabled_at: None,
                resolved_context_budget: None,
            });
        Ok(())
    }
}
