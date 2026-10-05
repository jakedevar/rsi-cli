impl Store {
    fn migrate_v028(&self, version: i32) -> Result<()> {
        // V28: Background task queue for async memory operations
        if version < 28 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS background_queue (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    work_unit_key   TEXT NOT NULL,
                    task_type       TEXT NOT NULL,
                    session_id      TEXT,
                    project_id      TEXT,
                    payload         TEXT NOT NULL DEFAULT '{}',
                    token_count     INTEGER NOT NULL DEFAULT 0,
                    status          TEXT NOT NULL DEFAULT 'pending',
                    priority        INTEGER NOT NULL DEFAULT 0,
                    attempts        INTEGER NOT NULL DEFAULT 0,
                    max_attempts    INTEGER NOT NULL DEFAULT 5,
                    error           TEXT,
                    created_at      TEXT NOT NULL,
                    claimed_at      TEXT,
                    completed_at    TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_bg_queue_work_unit
                    ON background_queue(work_unit_key, status);
                CREATE INDEX IF NOT EXISTS idx_bg_queue_status_type
                    ON background_queue(status, task_type);
                CREATE INDEX IF NOT EXISTS idx_bg_queue_session
                    ON background_queue(session_id, status);
                CREATE INDEX IF NOT EXISTS idx_bg_queue_claimed_stale
                    ON background_queue(claimed_at) WHERE status = 'claimed';",
            )?;
            tracing::info!("V28 migration complete: background_queue table");
            self.conn.execute("PRAGMA user_version = 28", [])?;
        }

        Ok(())
    }
}
