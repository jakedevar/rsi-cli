impl Store {
    fn migrate_v119(&self, version: i32) -> Result<()> {
        // V119: durable finite target-reclaim sweep and bounded selection indexes.
        if version < 119 {
            self.apply_target_reclaim_sweep_v119_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v119-target-reclaim-sweep-driver
    fn apply_target_reclaim_sweep_v119_migration(&self) -> Result<()> {
        use target_reclaim_sweep::TargetReclaimSweepV119MigrationFault as Fault;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 118 {
            return Err(DaemonError::Store(format!(
                "V119 requires exact V118 source, found V{active_version}"
            )));
        }
        let source_fingerprint = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if source_fingerprint != target_reclaim_sweep::V118_FULL_CATALOG_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V119 requires exact V118 catalog, found {source_fingerprint}"
            )));
        }
        target_reclaim_sweep::migration_fault(Fault::AfterPreflight)?;

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        target_reclaim_sweep::install_v119_table_and_row(&tx, &now)?;
        target_reclaim_sweep::migration_fault(Fault::AfterTableAndRow)?;
        target_reclaim_sweep::install_v119_indexes(&tx)?;
        target_reclaim_sweep::migration_fault(Fault::AfterIndexes)?;
        target_reclaim_sweep::install_v119_triggers(&tx)?;
        target_reclaim_sweep::migration_fault(Fault::AfterTriggers)?;
        target_reclaim_sweep::validate_v119_catalog(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V119 target reclaim migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V119 target reclaim migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        target_reclaim_sweep::migration_fault(Fault::AfterChecks)?;
        tx.execute("PRAGMA user_version = 119", [])?;
        target_reclaim_sweep::migration_fault(Fault::AfterUserVersion)?;
        let result_fingerprint = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if result_fingerprint != target_reclaim_sweep::V119_FULL_CATALOG_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V119 target reclaim catalog fingerprint mismatch: {result_fingerprint}"
            )));
        }
        target_reclaim_sweep::migration_fault(Fault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!(%result_fingerprint, "V119 migration complete: durable target reclaim sweep installed");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v119-target-reclaim-sweep-driver
}
