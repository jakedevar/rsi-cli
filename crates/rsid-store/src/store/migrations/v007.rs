impl Store {
    fn migrate_v007(&self, version: i32) -> Result<()> {
        // V7: Context snapshots for crash recovery
        if version < 7 {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS context_snapshots (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    tokens_used INTEGER NOT NULL,
                    created_at  TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_context_snapshots_session_latest
                    ON context_snapshots(session_id, created_at DESC);
                ",
            )?;
            self.conn.execute("PRAGMA user_version = 7", [])?;
        }

        Ok(())
    }
}
