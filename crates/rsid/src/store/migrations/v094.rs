impl Store {
    fn migrate_v094(&self, version: i32) -> Result<()> {
        // This compatibility repair is part of the shipped V93 source
        // catalog. Converge it before V94 so blank upgrades and databases
        // already at V93 enter the retained journal migration identically.
        if version < 94 {
            self.apply_source_worktree_settlement_v94_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v94-migration-driver
    fn apply_source_worktree_settlement_v94_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 93 {
            return Err(DaemonError::Store(format!(
                "V94 requires exact V93 source, found V{active_version}"
            )));
        }
        h1_v94_validate_v93_catalog(&tx)?;
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::AfterPreflight,
        )?;
        cohort_settlement::install_v94_schema(&tx)?;
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::AfterSchema,
        )?;
        cohort_settlement::validate_v94_catalog(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V94 settlement migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V94 settlement migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::AfterChecks,
        )?;
        tx.execute("PRAGMA user_version = 94", [])?;
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::AfterUserVersion,
        )?;
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::BeforeCommit,
        )?;
        tx.commit()?;
        tracing::info!("V94 migration complete: source-worktree settlement journal");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v94-migration-driver
}
