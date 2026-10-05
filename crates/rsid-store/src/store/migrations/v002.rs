impl Store {
    fn migrate_v002(&self, version: i32) -> Result<()> {
        // V2: Projects table and session project_id FK
        if version < 2 {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS projects (
                    id          TEXT PRIMARY KEY,
                    name        TEXT NOT NULL UNIQUE,
                    path        TEXT,
                    description TEXT,
                    color       TEXT DEFAULT '#89b4fa',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_projects_path ON projects(path);
                ",
            )?;

            // Add project_id to sessions (nullable FK)
            self.add_column_if_not_exists("sessions", "project_id", "TEXT")?;

            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_project_id ON sessions(project_id);",
            )?;

            self.conn.execute("PRAGMA user_version = 2", [])?;
        }

        Ok(())
    }
}
