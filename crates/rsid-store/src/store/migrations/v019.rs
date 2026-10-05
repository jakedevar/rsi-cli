impl Store {
    fn migrate_v019(&self, version: i32) -> Result<()> {
        // V19: Git branch tracking per session
        if version < 19 {
            self.add_column_if_not_exists("sessions", "git_branch", "TEXT")?;
            tracing::info!("V19 migration complete: added git_branch column to sessions");
            self.conn.execute("PRAGMA user_version = 19", [])?;
        }

        Ok(())
    }
}
