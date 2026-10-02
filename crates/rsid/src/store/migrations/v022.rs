impl Store {
    fn migrate_v022(&self, version: i32) -> Result<()> {
        // V22: Durable pending-archive flag
        if version < 22 {
            self.add_column_if_not_exists(
                "sessions",
                "pending_archive",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            tracing::info!("V22 migration complete: added pending_archive column to sessions");
            self.conn.execute("PRAGMA user_version = 22", [])?;
        }

        Ok(())
    }
}
