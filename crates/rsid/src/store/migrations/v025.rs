impl Store {
    fn migrate_v025(&self, version: i32) -> Result<()> {
        // V25: Per-session rotation toggle
        if version < 25 {
            self.add_column_if_not_exists("sessions", "rotation_disabled_at", "TEXT")?;
            tracing::info!("V25 migration complete: added rotation_disabled_at column to sessions");
            self.conn.execute("PRAGMA user_version = 25", [])?;
        }

        Ok(())
    }
}
