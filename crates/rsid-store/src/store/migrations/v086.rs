impl Store {
    fn migrate_v086(&self, version: i32) -> Result<()> {
        if version < 86 {
            // V86 is a real upgrade, not an amendment to the V85 creation
            // block.  A V85 database can already contain immutable origin
            // history, so rebuild the five mutually-referencing relations in
            // one deferred-FK transaction and copy every column byte-for-byte.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 85 {
                return Err(DaemonError::Store(format!(
                    "V86 requires exact V85 source, found V{active_version}"
                )));
            }
            h1_v86_migration_fault(H1V86MigrationFault::AfterPreflight)?;
            tx.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
            tx.execute_batch(origin_authority::V86_REBUILD_RENAME_SQL)?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterRename)?;
            tx.execute_batch(&origin_authority::v86_catalog_table_sql())?;
            tx.execute_batch(origin_authority::V86_REBUILD_COPY_AND_DROP_SQL)?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterCopy)?;
            tx.execute_batch(origin_authority::V85_INDEX_SQL)?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterIndexes)?;
            tx.execute_batch(origin_authority::V85_TRIGGER_SQL)?;
            tx.execute_batch(origin_authority::V86_TRIGGER_SQL)?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterTriggers)?;
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V86 execution-origin catalog foreign-key check failed".into(),
                ));
            }
            h1_v86_migration_fault(H1V86MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 86", [])?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterUserVersion)?;
            h1_v86_migration_fault(H1V86MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V86 migration complete: request-key provenance rebuild");
        }

        Ok(())
    }
}
