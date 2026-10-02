impl Store {
    fn migrate_v039(&self, version: i32) -> Result<()> {
        // V39: Agent sandbox columns (filesystem-only; process isolation reserved).
        if version < 39 {
            self.add_column_if_not_exists("sessions", "sandbox_kind", "TEXT")?;
            self.add_column_if_not_exists("sessions", "sandbox_root", "TEXT")?;
            self.add_column_if_not_exists("sessions", "sandbox_branch", "TEXT")?;
            self.add_column_if_not_exists("sessions", "sandbox_cleanup_state", "TEXT")?;
            tracing::info!("V39 migration complete: agent sandbox columns");
            self.conn.pragma_update(None, "user_version", 39)?;
        }

        Ok(())
    }
}
