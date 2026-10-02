impl Store {
    fn migrate_v095(&self, version: i32) -> Result<()> {
        if version < 95 {
            self.apply_source_worktree_settlement_v95_migration()?;
        } else {
            let tx = self.conn.unchecked_transaction()?;
            cohort_settlement::validate_v95_catalog(&tx)?;
            tx.commit()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v95-migration-driver
    fn apply_source_worktree_settlement_v95_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 94 {
            return Err(DaemonError::Store(format!(
                "V95 requires exact V94 source, found V{active_version}"
            )));
        }
        let v94_catalog = cohort_settlement::classify_v94_settlement_catalog(&tx)?;
        if v94_catalog == cohort_settlement::V94SettlementCatalog::Deployed {
            h1_v95_validate_v94_catalog(&tx)?;
        }
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V95 requires integrity_check=ok before hardening, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V95 preflight found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        cohort_settlement::validate_v95_source_rows(&tx)?;
        if v94_catalog == cohort_settlement::V94SettlementCatalog::Deployed {
            cohort_settlement::validate_deployed_v94_rows_for_current_target(&tx)?;
            cohort_settlement::bridge_deployed_v94_catalog(&tx)?;
            h1_v95_validate_v94_catalog(&tx)?;
        } else {
            h1_v95_validate_v94_catalog(&tx)?;
        }
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterPreflight,
        )?;
        cohort_settlement::drop_v95_replaceable_triggers(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterTriggerDrop,
        )?;
        cohort_settlement::normalize_v95_runs(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterNormalize,
        )?;
        cohort_settlement::install_v95_run_order(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterRunOrder,
        )?;
        cohort_settlement::install_v95_latest_run_pointer(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterLatestRunPointer,
        )?;
        cohort_settlement::install_v95_indexes(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterIndexes,
        )?;
        cohort_settlement::install_v95_triggers(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterTriggers,
        )?;
        cohort_settlement::validate_v95_catalog(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V95 settlement hardening requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V95 settlement hardening found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterChecks,
        )?;
        tx.execute("PRAGMA user_version = 95", [])?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterUserVersion,
        )?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::BeforeCommit,
        )?;
        tx.commit()?;
        tracing::info!("V95 migration complete: source-worktree settlement hardening");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v95-migration-driver
}
