impl Store {
    fn migrate_v030(&self, version: i32) -> Result<()> {
        // V30: Two-tier session summarization
        if version < 30 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_summaries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL,
                    content TEXT NOT NULL,
                    covers_through_sequence INTEGER NOT NULL,
                    token_count INTEGER NOT NULL,
                    created_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_session_summaries_session_kind
                    ON session_summaries(session_id, kind);
                CREATE INDEX IF NOT EXISTS idx_session_summaries_session_latest
                    ON session_summaries(session_id, kind, created_at DESC);",
            )?;
            tracing::info!("V30 migration complete: session_summaries table");
            self.conn.execute("PRAGMA user_version = 30", [])?;
        }

        Ok(())
    }
}
