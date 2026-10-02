impl Store {
    fn migrate_v139(&self, version: i32) -> Result<()> {
        // Issue #1000: cumulative full prompt tokens per session.
        if version < 139 {
            self.add_column_if_not_exists("sessions", "total_prompt_tokens", "INTEGER")?;
            self.conn.execute("PRAGMA user_version = 139", [])?;
        }

        Ok(())
    }
}
