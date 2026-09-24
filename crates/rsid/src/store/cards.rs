//! Entity card persistence operations.

use super::Store;
use crate::error::{DaemonError, Result};
use rsi_common::types::EntityCard;
use rusqlite::params;

impl Store {
    /// Get an entity card by type and ID.
    /// Returns None if no card exists for this entity.
    pub fn get_entity_card(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<EntityCard>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, entity_type, entity_id, facts, created_at, updated_at
             FROM entity_cards WHERE entity_type = ?1 AND entity_id = ?2",
        )?;

        let mut rows = stmt.query_map(params![entity_type, entity_id], |row| {
            let id_str: String = row.get(0)?;
            let entity_type: String = row.get(1)?;
            let entity_id: String = row.get(2)?;
            let facts_json: String = row.get(3)?;
            let created_at_str: String = row.get(4)?;
            let updated_at_str: String = row.get(5)?;
            Ok((
                id_str,
                entity_type,
                entity_id,
                facts_json,
                created_at_str,
                updated_at_str,
            ))
        })?;

        match rows.next() {
            Some(Ok((
                id_str,
                entity_type,
                entity_id,
                facts_json,
                created_at_str,
                updated_at_str,
            ))) => {
                let id = uuid::Uuid::parse_str(&id_str)
                    .map_err(|e| DaemonError::Store(format!("Invalid card UUID: {}", e)))?;
                let facts: Vec<String> = serde_json::from_str(&facts_json)
                    .map_err(|e| DaemonError::Store(format!("Invalid card facts JSON: {}", e)))?;
                let created_at = super::parse_timestamp(&created_at_str)
                    .map_err(|e| DaemonError::Store(format!("Invalid card created_at: {}", e)))?;
                let updated_at = super::parse_timestamp(&updated_at_str)
                    .map_err(|e| DaemonError::Store(format!("Invalid card updated_at: {}", e)))?;

                Ok(Some(EntityCard {
                    id,
                    entity_type,
                    entity_id,
                    facts,
                    created_at,
                    updated_at,
                }))
            }
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Upsert an entity card (INSERT OR REPLACE on unique constraint).
    pub fn upsert_entity_card(&self, card: &EntityCard) -> Result<()> {
        let facts_json = serde_json::to_string(&card.facts)
            .map_err(|e| DaemonError::Store(format!("Failed to serialize facts: {}", e)))?;

        self.conn.execute(
            "INSERT INTO entity_cards (id, entity_type, entity_id, facts, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(entity_type, entity_id) DO UPDATE SET
                facts = excluded.facts,
                updated_at = excluded.updated_at",
            params![
                card.id.to_string(),
                card.entity_type,
                card.entity_id,
                facts_json,
                card.created_at.to_rfc3339(),
                card.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Delete an entity card.
    pub fn delete_entity_card(&self, entity_type: &str, entity_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM entity_cards WHERE entity_type = ?1 AND entity_id = ?2",
            params![entity_type, entity_id],
        )?;
        Ok(())
    }

    /// List all entity cards of a given type.
    pub fn list_entity_cards(&self, entity_type: &str) -> Result<Vec<EntityCard>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, entity_type, entity_id, facts, created_at, updated_at
             FROM entity_cards WHERE entity_type = ?1 ORDER BY entity_id ASC",
        )?;

        let rows = stmt
            .query_map(params![entity_type], |row| {
                let id_str: String = row.get(0)?;
                let entity_type: String = row.get(1)?;
                let entity_id: String = row.get(2)?;
                let facts_json: String = row.get(3)?;
                let created_at_str: String = row.get(4)?;
                let updated_at_str: String = row.get(5)?;
                Ok((
                    id_str,
                    entity_type,
                    entity_id,
                    facts_json,
                    created_at_str,
                    updated_at_str,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(id_str, entity_type, entity_id, facts_json, created_at_str, updated_at_str)| {
                    let id = uuid::Uuid::parse_str(&id_str)
                        .map_err(|e| DaemonError::Store(format!("Invalid card UUID: {}", e)))?;
                    let facts: Vec<String> = serde_json::from_str(&facts_json).map_err(|e| {
                        DaemonError::Store(format!("Invalid card facts JSON: {}", e))
                    })?;
                    let created_at = super::parse_timestamp(&created_at_str).map_err(|e| {
                        DaemonError::Store(format!("Invalid card created_at: {}", e))
                    })?;
                    let updated_at = super::parse_timestamp(&updated_at_str).map_err(|e| {
                        DaemonError::Store(format!("Invalid card updated_at: {}", e))
                    })?;

                    Ok(EntityCard {
                        id,
                        entity_type,
                        entity_id,
                        facts,
                        created_at,
                        updated_at,
                    })
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn test_store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn test_card_insert_and_get() {
        let store = test_store();
        let card = EntityCard {
            id: Uuid::new_v4(),
            entity_type: "project".to_string(),
            entity_id: "proj-123".to_string(),
            facts: vec!["Uses Rust".to_string(), "TUI application".to_string()],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.upsert_entity_card(&card).unwrap();

        let loaded = store
            .get_entity_card("project", "proj-123")
            .unwrap()
            .expect("card should exist");
        assert_eq!(loaded.id, card.id);
        assert_eq!(loaded.entity_type, "project");
        assert_eq!(loaded.entity_id, "proj-123");
        assert_eq!(loaded.facts.len(), 2);
        assert_eq!(loaded.facts[0], "Uses Rust");
    }

    #[test]
    fn test_card_upsert_replaces_facts() {
        let store = test_store();
        let card = EntityCard {
            id: Uuid::new_v4(),
            entity_type: "user".to_string(),
            entity_id: "self".to_string(),
            facts: vec!["Original fact".to_string()],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.upsert_entity_card(&card).unwrap();

        // Upsert with new facts
        let updated = EntityCard {
            id: card.id,
            entity_type: "user".to_string(),
            entity_id: "self".to_string(),
            facts: vec!["Updated fact 1".to_string(), "Updated fact 2".to_string()],
            created_at: card.created_at,
            updated_at: Utc::now(),
        };
        store.upsert_entity_card(&updated).unwrap();

        let loaded = store
            .get_entity_card("user", "self")
            .unwrap()
            .expect("card should exist");
        assert_eq!(loaded.facts.len(), 2);
        assert_eq!(loaded.facts[0], "Updated fact 1");
    }

    #[test]
    fn test_card_get_nonexistent() {
        let store = test_store();
        let result = store.get_entity_card("project", "no-such-id").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_card_delete() {
        let store = test_store();
        let card = EntityCard {
            id: Uuid::new_v4(),
            entity_type: "project".to_string(),
            entity_id: "del-me".to_string(),
            facts: vec!["Fact".to_string()],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.upsert_entity_card(&card).unwrap();
        assert!(
            store
                .get_entity_card("project", "del-me")
                .unwrap()
                .is_some()
        );

        store.delete_entity_card("project", "del-me").unwrap();
        assert!(
            store
                .get_entity_card("project", "del-me")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_card_list_by_type() {
        let store = test_store();

        // Insert two project cards and one user card
        for (i, eid) in ["proj-a", "proj-b"].iter().enumerate() {
            let card = EntityCard {
                id: Uuid::new_v4(),
                entity_type: "project".to_string(),
                entity_id: eid.to_string(),
                facts: vec![format!("Fact for project {}", i)],
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            store.upsert_entity_card(&card).unwrap();
        }
        let user_card = EntityCard {
            id: Uuid::new_v4(),
            entity_type: "user".to_string(),
            entity_id: "self".to_string(),
            facts: vec!["User pref".to_string()],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.upsert_entity_card(&user_card).unwrap();

        let project_cards = store.list_entity_cards("project").unwrap();
        assert_eq!(project_cards.len(), 2);

        let user_cards = store.list_entity_cards("user").unwrap();
        assert_eq!(user_cards.len(), 1);
    }

    #[test]
    fn test_card_empty_facts() {
        let store = test_store();
        let card = EntityCard {
            id: Uuid::new_v4(),
            entity_type: "project".to_string(),
            entity_id: "empty".to_string(),
            facts: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.upsert_entity_card(&card).unwrap();

        let loaded = store
            .get_entity_card("project", "empty")
            .unwrap()
            .expect("card should exist");
        assert!(loaded.facts.is_empty());
    }
}
