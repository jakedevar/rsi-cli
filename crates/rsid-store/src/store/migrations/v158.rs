/// Issue #1461: `AgentRequestDeploy {interrupt_workers: true}`. Two columns on
/// the deploy catalog: `interrupt_workers` records that the manager accepted
/// interrupting workers still mid-turn once the drain hold is over, and
/// `interrupted_sessions` records the worker session ids the restart cut off
/// (a JSON array of canonical UUIDs, set once at the restart), so the outcome
/// wake can name them after the daemon came back. Both ride the row because
/// the deploy outlives daemon restarts and the outcome is written by the new
/// process. Existing rows read as `interrupt_workers = 0`, no record.
// RSI-RELEASED-MIGRATION-BEGIN: v158-deploy-interrupt-workers
const V158_INTERRUPT_WORKERS: &str = "INTEGER NOT NULL DEFAULT 0 CHECK(interrupt_workers IN (0,1))";
const V158_INTERRUPTED_SESSIONS: &str =
    "TEXT CHECK(interrupted_sessions IS NULL OR json_valid(interrupted_sessions))";
// RSI-RELEASED-MIGRATION-END: v158-deploy-interrupt-workers

impl Store {
    fn migrate_v158(&self, version: i32) -> Result<()> {
        if version < 158 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if prior != 157 {
                return Err(DaemonError::Store(format!(
                    "the deploy interrupt-workers columns require V157, found V{prior}"
                )));
            }
            add_column_if_not_exists_tx(
                &tx,
                "agent_deploys",
                "interrupt_workers",
                V158_INTERRUPT_WORKERS,
            )?;
            add_column_if_not_exists_tx(
                &tx,
                "agent_deploys",
                "interrupted_sessions",
                V158_INTERRUPTED_SESSIONS,
            )?;
            tx.pragma_update(None, "user_version", 158)?;
            tx.commit()?;
        }
        Ok(())
    }
}
