impl Store {
    fn migrate_v009(&self, version: i32) -> Result<()> {
        // V9: Model segments for mid-session model switching
        if version < 9 {
            tracing::info!("Applying migration V9: model_segments table");

            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS model_segments (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    from_sequence INTEGER NOT NULL,
                    to_sequence INTEGER,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS idx_model_segments_session
                    ON model_segments(session_id, from_sequence);",
            )?;

            self.add_column_if_not_exists("turn_metrics", "model", "TEXT")?;

            // Backfill: create initial segment for each session with a model
            self.conn.execute(
                "INSERT INTO model_segments (session_id, model_id, from_sequence, to_sequence, created_at)
                 SELECT id, model, 0, NULL, created_at
                 FROM sessions
                 WHERE model IS NOT NULL",
                [],
            )?;

            let segment_count: i64 =
                self.conn
                    .query_row("SELECT COUNT(*) FROM model_segments", [], |r| r.get(0))?;
            tracing::info!(
                "V9 migration complete: {} segments backfilled",
                segment_count
            );

            self.conn.execute("PRAGMA user_version = 9", [])?;
        }

        Ok(())
    }
}
