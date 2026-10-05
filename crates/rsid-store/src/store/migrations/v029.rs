impl Store {
    fn migrate_v029(&self, version: i32) -> Result<()> {
        // V29: Entity cards (project + user cards for context injection)
        if version < 29 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS entity_cards (
                    id          TEXT PRIMARY KEY,
                    entity_type TEXT NOT NULL,
                    entity_id   TEXT NOT NULL,
                    facts       TEXT NOT NULL DEFAULT '[]',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    UNIQUE(entity_type, entity_id)
                );
                CREATE INDEX IF NOT EXISTS idx_entity_cards_lookup
                    ON entity_cards(entity_type, entity_id);",
            )?;
            tracing::info!("V29 migration complete: entity_cards table");
            self.conn.execute("PRAGMA user_version = 29", [])?;
        }

        Ok(())
    }
}
