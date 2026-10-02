impl Store {
    fn migrate_v006(&self, version: i32) -> Result<()> {
        // V6: Provider support (Claude/Codex)
        if version < 6 {
            self.add_column_if_not_exists(
                "sessions",
                "provider",
                "TEXT NOT NULL DEFAULT 'Claude'",
            )?;
            self.conn.execute("PRAGMA user_version = 6", [])?;
        }

        Ok(())
    }
}
