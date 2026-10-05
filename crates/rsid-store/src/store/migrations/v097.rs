impl Store {
    fn migrate_v097(&self, version: i32) -> Result<()> {
        // V97: guarded Issue CRUD needs durable optimistic versions plus an
        // append-only receipt/audit stream. This is an exact-source,
        // version-last migration: a failure at any boundary leaves a usable
        // V96 catalog and the old daemon may reopen it safely.
        if version < 97 {
            self.apply_issue_v97_migration()?;
        }

        Ok(())
    }

    fn apply_issue_v97_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 96 {
            return Err(DaemonError::Store(format!(
                "V97 requires exact V96 source, found V{active_version}"
            )));
        }
        cohort_settlement::validate_v95_catalog(&tx)?;
        validate_v96_tool_correlation_catalog(&tx)?;
        let witness = issues::validate_v96_issue_source_catalog(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterPreflight)?;
        issues::add_v97_issue_columns(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterIssueColumns)?;
        issues::create_v97_issue_event_table(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterEventTable)?;
        issues::backfill_v97_issue_events(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterBaselineCopy)?;
        issues::create_v97_issue_indexes(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterIndexes)?;
        issues::create_v97_issue_triggers(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterTriggers)?;
        issues::validate_v97_catalog(&tx, &witness)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterParity)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V97 Issue migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V97 Issue migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        issue_v97_migration_fault(IssueV97MigrationFault::AfterChecks)?;
        tx.execute("PRAGMA user_version = 97", [])?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterUserVersion)?;
        issue_v97_migration_fault(IssueV97MigrationFault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!("V97 migration complete: guarded Issue audit ledger");
        Ok(())
    }
}
