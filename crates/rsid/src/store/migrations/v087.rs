impl Store {
    fn migrate_v087(&self, version: i32) -> Result<()> {
        if version < 87 {
            // V87 is a trigger-only replacement over the exact deployed V86
            // catalog. Historical V85/V86 reconstruction stays byte-stable.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 86 {
                return Err(DaemonError::Store(format!(
                    "V87 requires exact V86 source, found V{active_version}"
                )));
            }
            origin_authority::validate_v86_session_trigger(&tx)?;
            h1_v87_migration_fault(H1V87MigrationFault::AfterPreflight)?;
            tx.execute_batch("DROP TRIGGER sessions_execution_origin_write_guard;")?;
            h1_v87_migration_fault(H1V87MigrationFault::AfterDrop)?;
            tx.execute_batch(origin_authority::V87_SESSION_TRIGGER_SQL)?;
            h1_v87_migration_fault(H1V87MigrationFault::AfterCreate)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V87 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V87 Session-fence catalog foreign-key check failed".into(),
                ));
            }
            h1_v87_migration_fault(H1V87MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 87", [])?;
            h1_v87_migration_fault(H1V87MigrationFault::AfterUserVersion)?;
            h1_v87_migration_fault(H1V87MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V87 migration complete: Session fence replacement");
        }

        Ok(())
    }
}
