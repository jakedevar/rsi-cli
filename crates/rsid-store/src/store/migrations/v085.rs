impl Store {
    fn migrate_v085(&self, version: i32) -> Result<()> {
        if version < 85 {
            // V85 is intentionally private to the daemon.  The two sessions
            // columns are transaction fences, not part of the Session wire
            // contract; all durable protocol state lives in the catalog below.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 84 {
                return Err(DaemonError::Store(format!(
                    "V85 requires exact V84 source, found V{active_version}"
                )));
            }
            origin_authority::validate_v84_catalog(&tx)?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterPreflight)?;
            tx.execute_batch(&origin_authority::v85_table_sql())?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterTables)?;
            origin_authority::seed_v84_authorities(&tx)?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterBackfill)?;
            tx.execute_batch(origin_authority::V85_INDEX_SQL)?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterIndexes)?;
            tx.execute_batch(origin_authority::V85_TRIGGER_SQL)?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterTriggers)?;
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V85 execution-origin catalog foreign-key check failed".into(),
                ));
            }
            h1_v85_migration_fault(H1V85MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 85", [])?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterUserVersion)?;
            h1_v85_migration_fault(H1V85MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V85 migration complete: execution-origin catalog foundation");
        }

        Ok(())
    }
}
