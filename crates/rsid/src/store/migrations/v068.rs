impl Store {
    fn migrate_v068(&self, version: i32) -> Result<()> {
        if version < 68 {
            // scheduled_jobs was created in V35. Guard against artificial test
            // scenarios (e.g. blank DB forced to user_version=38) where V35 was
            // bypassed and the table doesn't exist yet. In production every
            // V38+ database already has scheduled_jobs.
            let table_exists: bool = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='scheduled_jobs'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if table_exists {
                self.add_column_if_not_exists(
                    "scheduled_jobs",
                    "wake_mode",
                    "TEXT NOT NULL DEFAULT 'fresh'",
                )?;
                self.add_column_if_not_exists("scheduled_jobs", "wake_session_id", "TEXT")?;
            }
            tracing::info!("V68 migration complete: scheduled_jobs.wake_mode + wake_session_id");
            self.conn.pragma_update(None, "user_version", 68)?;
        }

        Ok(())
    }
}
