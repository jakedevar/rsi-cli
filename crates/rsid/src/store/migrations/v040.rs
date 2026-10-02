impl Store {
    fn migrate_v040(&self, version: i32) -> Result<()> {
        // V40: Hierarchical parent_id (organizational tree: Group/Epic containers).
        if version < 40 {
            self.add_column_if_not_exists("sessions", "parent_id", "TEXT")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_parent_id ON sessions(parent_id)",
                [],
            )?;
            tracing::info!("V40 migration complete: hierarchical parent_id column + index");
            self.conn.pragma_update(None, "user_version", 40)?;
        }

        Ok(())
    }
}
