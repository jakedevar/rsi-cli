impl Store {
    fn migrate_v010(&self, version: i32) -> Result<()> {
        if version < 10 {
            self.conn.execute(
                "UPDATE sessions SET status = 'Archived' WHERE status = 'Rotated'",
                [],
            )?;
            let converted: i64 = self.conn.query_row("SELECT changes()", [], |r| r.get(0))?;
            tracing::info!(
                "V10 migration complete: {} Rotated sessions converted to Archived",
                converted
            );
            self.conn.execute("PRAGMA user_version = 10", [])?;
        }

        Ok(())
    }
}
