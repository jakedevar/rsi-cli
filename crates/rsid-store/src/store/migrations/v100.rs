impl Store {
    fn migrate_v100(&self, version: i32) -> Result<()> {
        // V100: persist the active context-window evidence without inventing
        // provenance for historical numeric values.
        if version < 100 {
            self.apply_context_window_provenance_v100_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v100-context-window-provenance
    fn apply_context_window_provenance_v100_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 99 {
            return Err(DaemonError::Store(format!(
                "V100 requires exact V99 source, found V{active_version}"
            )));
        }
        tx.execute_batch(
            "ALTER TABLE sessions ADD COLUMN context_window_source TEXT
                 CHECK (context_window_source IS NULL OR context_window_source IN (
                     'official_documentation', 'provider_catalog', 'configured',
                     'runtime_telemetry', 'repository_fallback', 'legacy_unverified'
                 ));
             ALTER TABLE sessions ADD COLUMN context_window_source_version TEXT;
             ALTER TABLE sessions ADD COLUMN context_window_source_digest TEXT;
             ALTER TABLE sessions ADD COLUMN context_window_observed_at TEXT;
             UPDATE sessions
                SET context_window_source='legacy_unverified'
              WHERE context_window IS NOT NULL;",
        )?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V100 context-window provenance migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V100 context-window provenance migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 100", [])?;
        tx.commit()?;
        tracing::info!("V100 migration complete: context-window provenance");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v100-context-window-provenance
}
