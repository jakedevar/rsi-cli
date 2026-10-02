impl Store {
    fn migrate_v013(&self, version: i32) -> Result<()> {
        // V13: Haiku-generated or user-set session title
        if version < 13 {
            self.add_column_if_not_exists("sessions", "title", "TEXT")?;
            tracing::info!("V13 migration complete: added title column to sessions");
            self.conn.execute("PRAGMA user_version = 13", [])?;
        }

        Ok(())
    }
}
