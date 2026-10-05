impl Store {
    fn migrate_v012(&self, version: i32) -> Result<()> {
        // V12: Daemon-counted token columns (monotonically increasing, no API dependency)
        if version < 12 {
            self.add_column_if_not_exists("sessions", "daemon_input_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "daemon_output_tokens", "INTEGER")?;
            tracing::info!(
                "V12 migration complete: added daemon_input_tokens/daemon_output_tokens to sessions"
            );
            self.conn.execute("PRAGMA user_version = 12", [])?;
        }

        Ok(())
    }
}
