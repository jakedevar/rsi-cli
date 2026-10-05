impl Store {
    fn migrate_v015(&self, version: i32) -> Result<()> {
        // V15: Workflows table and session workflow_id FK
        if version < 15 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS workflows (
                    id          TEXT PRIMARY KEY,
                    title       TEXT NOT NULL,
                    stage       TEXT NOT NULL DEFAULT 'Research',
                    artifact_path TEXT,
                    definition_json TEXT,
                    project_id  TEXT REFERENCES projects(id),
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_workflows_project ON workflows(project_id);
                CREATE INDEX IF NOT EXISTS idx_workflows_stage ON workflows(stage);",
            )?;

            self.add_column_if_not_exists("sessions", "workflow_id", "TEXT")?;
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_workflow ON sessions(workflow_id);",
            )?;

            tracing::info!("V15 migration complete: workflows table + session workflow_id");
            self.conn.execute("PRAGMA user_version = 15", [])?;
        }

        Ok(())
    }
}
