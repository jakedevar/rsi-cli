impl Store {
    fn migrate_v035(&self, version: i32) -> Result<()> {
        // V35: Scheduled jobs
        if version < 35 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS scheduled_jobs (
                    id              TEXT PRIMARY KEY,
                    name            TEXT NOT NULL,
                    message         TEXT NOT NULL,
                    schedule_json   TEXT NOT NULL,
                    last_fired_at   TEXT,
                    next_fire_at    TEXT NOT NULL,
                    enabled         INTEGER NOT NULL DEFAULT 1,
                    working_dir     TEXT,
                    provider        TEXT,
                    model           TEXT,
                    project_id      TEXT,
                    created_at      TEXT NOT NULL,
                    updated_at      TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_enabled_next
                    ON scheduled_jobs(enabled, next_fire_at)
                    WHERE enabled = 1;
                CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_project
                    ON scheduled_jobs(project_id);",
            )?;
            tracing::info!("V35 migration complete: scheduled_jobs table");
            self.conn.pragma_update(None, "user_version", 35)?;
        }

        Ok(())
    }
}
