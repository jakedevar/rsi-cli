impl Store {
    fn migrate_v027(&self, version: i32) -> Result<()> {
        // V27: Persist full workflow definitions alongside metadata.
        if version < 27 {
            self.add_column_if_not_exists("workflows", "definition_json", "TEXT")?;
            tracing::info!("V27 migration complete: added definition_json to workflows");
            self.conn.execute("PRAGMA user_version = 27", [])?;
        }

        Ok(())
    }
}
