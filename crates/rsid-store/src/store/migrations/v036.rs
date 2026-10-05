impl Store {
    fn migrate_v036(&self, version: i32) -> Result<()> {
        // V36: Add scheduled_job_id to sessions
        if version < 36 {
            self.add_column_if_not_exists("sessions", "scheduled_job_id", "TEXT")?;
            tracing::info!("V36 migration complete: sessions.scheduled_job_id");
            self.conn.pragma_update(None, "user_version", 36)?;
        }

        Ok(())
    }
}
