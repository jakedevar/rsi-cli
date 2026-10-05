impl Store {
    fn migrate_v008(&self, version: i32) -> Result<()> {
        // V8: Add handoff_filepath for automatic rotation
        if version < 8 {
            self.add_column_if_not_exists("sessions", "handoff_filepath", "TEXT")?;
            self.conn.execute("PRAGMA user_version = 8", [])?;
        }

        Ok(())
    }
}
