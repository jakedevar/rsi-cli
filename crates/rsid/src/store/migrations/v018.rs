impl Store {
    fn migrate_v018(&self, version: i32) -> Result<()> {
        // V18: Rotation events table for debugging context rotation lifecycle
        if version < 18 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS rotation_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    rotation_id TEXT NOT NULL,
                    phase TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    metadata TEXT,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_rotation_events_session ON rotation_events(session_id);
                CREATE INDEX IF NOT EXISTS idx_rotation_events_rotation ON rotation_events(rotation_id);
                PRAGMA user_version = 18;",
            )?;
            tracing::info!("V18 migration complete: rotation_events table");
        }

        Ok(())
    }
}
