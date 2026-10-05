impl Store {
    fn migrate_v020(&self, version: i32) -> Result<()> {
        // V20: Durable work context — persistent active task description
        if version < 20 {
            self.add_column_if_not_exists("sessions", "active_task", "TEXT")?;
            tracing::info!("V20 migration complete: added active_task column to sessions");
            self.conn.execute("PRAGMA user_version = 20", [])?;
        }

        Ok(())
    }
}
