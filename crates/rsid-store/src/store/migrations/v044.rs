impl Store {
    fn migrate_v044(&self, version: i32) -> Result<()> {
        // V44: Eval-session tagging (RSI-006).
        // `is_eval=1` rows are excluded from production analytics and from default
        // `ListSessions` projections. Default 0 keeps every pre-V44 row in the
        // production set without backfill. Boolean encoded as INTEGER per SQLite
        // convention; matches the `pending_archive`, `test_passed`, `clippy_passed`
        // pattern.
        if version < 44 {
            self.add_column_if_not_exists("sessions", "is_eval", "INTEGER NOT NULL DEFAULT 0")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_is_eval ON sessions(is_eval)",
                [],
            )?;
            tracing::info!("V44 migration complete: is_eval column + idx_sessions_is_eval");
            self.conn.pragma_update(None, "user_version", 44)?;
        }

        Ok(())
    }
}
