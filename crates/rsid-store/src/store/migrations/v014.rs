impl Store {
    fn migrate_v014(&self, version: i32) -> Result<()> {
        // V14: Pipeline artifact path (Write-tool detected output file)
        if version < 14 {
            self.add_column_if_not_exists("sessions", "pipeline_artifact", "TEXT")?;
            tracing::info!("V14 migration complete: added pipeline_artifact column to sessions");
            self.conn.execute("PRAGMA user_version = 14", [])?;
        }

        Ok(())
    }
}
