impl Store {
    fn migrate_v024(&self, version: i32) -> Result<()> {
        // V24: Testing-needed session marker
        if version < 24 {
            self.add_column_if_not_exists("sessions", "testing_needed_at", "TEXT")?;
            tracing::info!("V24 migration complete: added testing_needed_at column to sessions");
            self.conn.execute("PRAGMA user_version = 24", [])?;
        }

        Ok(())
    }
}
