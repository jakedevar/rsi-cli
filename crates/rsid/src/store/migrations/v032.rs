impl Store {
    fn migrate_v032(&self, version: i32) -> Result<()> {
        // V32: Observations table for dream consolidation (Dreamer system)
        if version < 32 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS observations (
                    id              TEXT PRIMARY KEY,
                    session_id      TEXT NOT NULL,
                    project_id      TEXT,
                    level           TEXT NOT NULL DEFAULT 'explicit',
                    content         TEXT NOT NULL,
                    source_ids      TEXT DEFAULT '[]',
                    confidence      TEXT,
                    times_derived   INTEGER NOT NULL DEFAULT 1,
                    deleted_at      TEXT,
                    created_at      TEXT NOT NULL,
                    updated_at      TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_observations_session ON observations(session_id);
                CREATE INDEX IF NOT EXISTS idx_observations_project ON observations(project_id);
                CREATE INDEX IF NOT EXISTS idx_observations_level ON observations(level);
                CREATE INDEX IF NOT EXISTS idx_observations_deleted ON observations(deleted_at);
                CREATE INDEX IF NOT EXISTS idx_observations_created ON observations(created_at);

                CREATE TABLE IF NOT EXISTS dream_state (
                    key             TEXT PRIMARY KEY,
                    value           TEXT NOT NULL,
                    updated_at      TEXT NOT NULL
                );",
            )?;
            tracing::info!("V32 migration complete: observations + dream_state tables");
            self.conn.execute("PRAGMA user_version = 32", [])?;
        }

        Ok(())
    }
}
