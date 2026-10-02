impl Store {
    fn migrate_v003(&self, version: i32) -> Result<()> {
        // V3: Pinned sessions
        if version < 3 {
            self.add_column_if_not_exists("sessions", "pinned_at", "TEXT")?;
            self.conn.execute("PRAGMA user_version = 3", [])?;
        }

        Ok(())
    }
}
