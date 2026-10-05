impl Store {
    fn migrate_v031(&self, version: i32) -> Result<()> {
        // V31: Retry tracking columns
        if version < 31 {
            self.add_column_if_not_exists("sessions", "retry_attempt", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "max_retries", "INTEGER")?;
            tracing::info!("V31 migration complete: added retry_attempt/max_retries to sessions");
            self.conn.execute("PRAGMA user_version = 31", [])?;
        }

        Ok(())
    }
}
