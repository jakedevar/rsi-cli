impl Store {
    fn migrate_v041(&self, version: i32) -> Result<()> {
        // V41: WaitingApproval duration accumulator (server-side telemetry correctness).
        // Nullable for symmetry with V38 outcome proxies: None = unmeasured (pre-V41 row
        // or session that never reached finalize), Some(0) = measured-but-never-waited.
        if version < 41 {
            self.add_column_if_not_exists("sessions", "approval_wait_ms", "INTEGER")?;
            tracing::info!("V41 migration complete: approval_wait_ms column");
            self.conn.pragma_update(None, "user_version", 41)?;
        }

        Ok(())
    }
}
