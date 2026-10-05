impl Store {
    fn migrate_v033(&self, version: i32) -> Result<()> {
        // V33: Issue tracker integration
        if version < 33 {
            self.add_column_if_not_exists("sessions", "issue_identifier", "TEXT")?;
            self.add_column_if_not_exists("sessions", "issue_url", "TEXT")?;
            self.add_column_if_not_exists("sessions", "issue_tracker_id", "TEXT")?;

            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS issue_tracker_dispatches (
                    issue_id TEXT NOT NULL PRIMARY KEY,
                    issue_identifier TEXT NOT NULL,
                    tracker TEXT NOT NULL DEFAULT 'linear',
                    session_id TEXT NOT NULL,
                    dispatched_at TEXT NOT NULL,
                    last_reconciled_at TEXT,
                    terminal_state TEXT,
                    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000000Z', 'now'))
                );

                CREATE INDEX IF NOT EXISTS idx_issue_dispatches_session
                    ON issue_tracker_dispatches(session_id);
                CREATE INDEX IF NOT EXISTS idx_issue_dispatches_tracker
                    ON issue_tracker_dispatches(tracker);",
            )?;

            tracing::info!("V33 migration complete: issue tracker columns + dispatch table");
            self.conn.execute("PRAGMA user_version = 33", [])?;
        }

        Ok(())
    }
}
