impl Store {
    fn migrate_v001(&self, version: i32) -> Result<()> {
        // V1: Session metadata columns + turn_metrics table
        if version < 1 {
            self.add_column_if_not_exists("sessions", "cost_usd", "REAL")?;
            self.add_column_if_not_exists("sessions", "duration_ms", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "num_turns", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "model", "TEXT")?;
            self.add_column_if_not_exists("sessions", "input_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "output_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "context_window", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "total_input_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "total_output_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "total_cache_creation_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "total_cache_read_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "stop_reason", "TEXT")?;

            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS turn_metrics (
                    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id              TEXT NOT NULL REFERENCES sessions(id),
                    turn_number             INTEGER NOT NULL,
                    input_tokens            INTEGER NOT NULL DEFAULT 0,
                    cache_creation_tokens   INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens       INTEGER NOT NULL DEFAULT 0,
                    output_tokens           INTEGER NOT NULL DEFAULT 0,
                    stop_reason             TEXT,
                    tools_used              TEXT,
                    tool_count              INTEGER NOT NULL DEFAULT 0,
                    created_at              TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_turn_metrics_session
                    ON turn_metrics(session_id);
                CREATE INDEX IF NOT EXISTS idx_turn_metrics_session_turn
                    ON turn_metrics(session_id, turn_number);
                ",
            )?;

            self.conn.execute("PRAGMA user_version = 1", [])?;
        }

        Ok(())
    }
}
