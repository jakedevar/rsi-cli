impl Store {
    fn migrate_v011(&self, version: i32) -> Result<()> {
        if version < 11 {
            self.add_column_if_not_exists(
                "sessions",
                "rotation_depth",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            tracing::info!("V11 migration complete: added rotation_depth column to sessions");
            self.conn.execute("PRAGMA user_version = 11", [])?;
        }

        Ok(())
    }
}
