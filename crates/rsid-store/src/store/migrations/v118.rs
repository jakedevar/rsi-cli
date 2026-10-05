impl Store {
    fn migrate_v118(&self, version: i32) -> Result<()> {
        // V118: bound obsolete-route retirement by job and route before sequence.
        if version < 118 {
            self.apply_manager_notice_retirement_v118_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v118-manager-notice-retirement-driver
    fn apply_manager_notice_retirement_v118_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 117 {
            return Err(DaemonError::Store(format!(
                "V118 requires exact V117 source, found V{active_version}"
            )));
        }
        tx.execute_batch(
            "CREATE INDEX harness_manager_notices_pending_job_route_sequence
             ON harness_manager_notices(job_id,project_id,manager_session_id,
                 scope_version,direction,sequence)
             WHERE retired_at IS NULL AND settled_at IS NULL;",
        )?;
        tx.execute("PRAGMA user_version = 118", [])?;
        tx.commit()?;
        tracing::info!("V118 migration complete: bounded manager notice retirement installed");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v118-manager-notice-retirement-driver
}
