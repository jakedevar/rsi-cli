impl Store {
    fn migrate_v026(&self, version: i32) -> Result<()> {
        // V26: Effort parameter for Claude sessions
        if version < 26 {
            self.add_column_if_not_exists("sessions", "effort", "TEXT")?;
            tracing::info!("V26 migration complete: added effort column to sessions");
            self.conn.execute("PRAGMA user_version = 26", [])?;
        }

        Ok(())
    }
}
