impl Store {
    fn migrate_v023(&self, version: i32) -> Result<()> {
        // V23: Project context files for structured context injection
        if version < 23 {
            self.add_column_if_not_exists("projects", "context_files", "TEXT")?;
            tracing::info!("V23 migration complete: added context_files column to projects");
            self.conn.execute("PRAGMA user_version = 23", [])?;
        }

        Ok(())
    }
}
