impl Store {
    fn migrate_v005(&self, version: i32) -> Result<()> {
        // V5: Context rotation support (continued_from FK)
        if version < 5 {
            self.add_column_if_not_exists("sessions", "continued_from", "TEXT")?;
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_continued_from ON sessions(continued_from);",
            )?;
            self.conn.execute("PRAGMA user_version = 5", [])?;
        }

        Ok(())
    }
}
