impl Store {
    fn migrate_v067(&self, version: i32) -> Result<()> {
        if version < 67 {
            self.add_column_if_not_exists("sessions", "pending_question_json", "TEXT")?;
            tracing::info!("V67 migration complete: sessions.pending_question_json");
            self.conn.pragma_update(None, "user_version", 67)?;
        }

        Ok(())
    }
}
