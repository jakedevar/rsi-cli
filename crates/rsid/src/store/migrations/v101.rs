impl Store {
    fn migrate_v101(&self, version: i32) -> Result<()> {
        // V101: preserve the raw configured window independently from the
        // provider's effective active denominator.
        if version < 101 {
            self.apply_configured_context_window_v101_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v101-configured-context-window
    fn apply_configured_context_window_v101_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 100 {
            return Err(DaemonError::Store(format!(
                "V101 requires exact V100 source, found V{active_version}"
            )));
        }
        tx.execute_batch(
            "ALTER TABLE sessions ADD COLUMN context_window_configured_tokens INTEGER
                 CHECK (context_window_configured_tokens IS NULL OR
                        context_window_configured_tokens > 0);",
        )?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V101 configured context-window migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V101 configured context-window migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 101", [])?;
        tx.commit()?;
        tracing::info!("V101 migration complete: configured context-window intent");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v101-configured-context-window
}
