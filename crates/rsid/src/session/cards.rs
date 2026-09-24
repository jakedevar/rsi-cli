//! Entity card operations on SessionManager.

use super::SessionManager;
use crate::error::{DaemonError, Result};
use chrono::Utc;
use rsi_common::types::EntityCard;
use uuid::Uuid;

impl SessionManager {
    /// Get an entity card by type and ID.
    pub async fn get_entity_card(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<EntityCard>> {
        let store = self.store.clone();
        let et = entity_type.to_string();
        let eid = entity_id.to_string();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_entity_card(&et, &eid)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    /// Set (create or replace) an entity card's facts.
    /// Enforces the MAX_FACTS limit.
    pub async fn set_entity_card(
        &self,
        entity_type: String,
        entity_id: String,
        facts: Vec<String>,
    ) -> Result<EntityCard> {
        if facts.len() > EntityCard::MAX_FACTS {
            return Err(DaemonError::Rpc(format!(
                "Card exceeds maximum of {} facts (got {})",
                EntityCard::MAX_FACTS,
                facts.len()
            )));
        }

        // Load existing to preserve ID and created_at
        let existing = self.get_entity_card(&entity_type, &entity_id).await?;

        let now = Utc::now();
        let card = EntityCard {
            id: existing.as_ref().map(|c| c.id).unwrap_or_else(Uuid::new_v4),
            entity_type,
            entity_id,
            facts,
            created_at: existing.as_ref().map(|c| c.created_at).unwrap_or(now),
            updated_at: now,
        };

        self.persistence.upsert_entity_card(card.clone()).await?;
        Ok(card)
    }
}
