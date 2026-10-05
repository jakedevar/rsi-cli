impl Store {
    fn migrate_v089(&self, version: i32) -> Result<()> {
        if version < 89 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 88 {
                return Err(DaemonError::Store(format!(
                    "V89 requires exact V88 source, found V{active_version}"
                )));
            }
            capacity_recovery::validate_v88_source(&tx)?;
            let source_integrity: String =
                tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if source_integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V89 requires integrity_check=ok for exact V88 source, got {source_integrity}"
                )));
            }
            let source_foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if source_foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V89 requires clean V88 foreign keys, found {source_foreign_key_errors} violation(s)"
                )));
            }
            v89_migration_fault(V89MigrationFault::AfterPreflight)?;
            tx.execute_batch(capacity_recovery::V89_TABLE_SQL)?;
            v89_migration_fault(V89MigrationFault::AfterTables)?;
            tx.execute_batch(capacity_recovery::V89_INDEX_SQL)?;
            v89_migration_fault(V89MigrationFault::AfterIndexes)?;
            tx.execute_batch(capacity_recovery::V89_TRIGGER_SQL)?;
            v89_migration_fault(V89MigrationFault::AfterTriggers)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V89 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V89 provider-capacity catalog foreign-key check failed".into(),
                ));
            }
            v89_migration_fault(V89MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 89", [])?;
            v89_migration_fault(V89MigrationFault::AfterUserVersion)?;
            v89_migration_fault(V89MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V89 migration complete: provider-capacity recovery ledger");
        }

        Ok(())
    }
}
