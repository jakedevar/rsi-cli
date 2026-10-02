impl Store {
    fn migrate_v091(&self, version: i32) -> Result<()> {
        if version < 91 {
            // V91 changes no durable DDL. It authenticates the exact two-shape
            // V90 whole catalog and refuses any active predecessor that cannot
            // complete its unchanged V87 Session-fence sequence.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 90 {
                return Err(DaemonError::Store(format!(
                    "V91 requires exact V90 source, found V{active_version}"
                )));
            }
            h1_v91_validate_v90_catalog(&tx)?;
            h1_validate_exact_active_predecessors(&tx, 91, 90)?;
            h1_v91_migration_fault(H1V91MigrationFault::AfterPreflight)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V91 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V91 exact-predecessor foreign-key check found {foreign_key_errors} violation(s)"
                )));
            }
            h1_v91_migration_fault(H1V91MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 91", [])?;
            h1_v91_migration_fault(H1V91MigrationFault::AfterUserVersion)?;
            h1_v91_migration_fault(H1V91MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V91 migration complete: exact execution-origin predecessors");
        }

        Ok(())
    }
}
