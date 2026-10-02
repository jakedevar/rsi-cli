impl Store {
    fn migrate_v017(&self, version: i32) -> Result<()> {
        // V17: LLM-generated paragraph description for sessions
        if version < 17 {
            self.add_column_if_not_exists("sessions", "description", "TEXT")?;
            tracing::info!("V17 migration complete: added description column to sessions");
            self.conn.execute("PRAGMA user_version = 17", [])?;
        }

        Ok(())
    }
}
