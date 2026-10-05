impl Store {
    fn migrate_v004(&self, version: i32) -> Result<()> {
        // V4: Session kind for TaskRabbit support
        if version < 4 {
            self.add_column_if_not_exists(
                "sessions",
                "session_kind",
                "TEXT NOT NULL DEFAULT 'Standard'",
            )?;
            self.conn.execute("PRAGMA user_version = 4", [])?;
        }

        Ok(())
    }
}
