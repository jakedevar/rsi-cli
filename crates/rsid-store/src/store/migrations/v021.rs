impl Store {
    fn migrate_v021(&self, version: i32) -> Result<()> {
        // V21: Session grouping (lightweight convoy)
        if version < 21 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_groups (
                    id          TEXT PRIMARY KEY,
                    name        TEXT NOT NULL,
                    description TEXT,
                    project_id  TEXT,
                    color       TEXT NOT NULL DEFAULT '#cba6f7',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_session_groups_project ON session_groups(project_id);",
            )?;

            self.add_column_if_not_exists("sessions", "group_id", "TEXT")?;
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_group_id ON sessions(group_id);",
            )?;

            tracing::info!("V21 migration complete: session_groups table + sessions.group_id");
            self.conn.execute("PRAGMA user_version = 21", [])?;
        }

        Ok(())
    }
}
