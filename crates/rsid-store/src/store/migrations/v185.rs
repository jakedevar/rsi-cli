// rsi-migration: additive
// Hand-enqueued sources retain the Issue they implement across manager rotation.
impl Store {
    fn migrate_v185(&self, version: i32) -> Result<()> {
        if version < 185 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "ALTER TABLE rolling_queue_entries ADD COLUMN issue INTEGER CHECK(issue IS NULL OR issue>0);
                 CREATE INDEX rolling_queue_live_issue ON rolling_queue_entries(project_id,issue,sequence)
                 WHERE issue IS NOT NULL AND state IN ('queued','admitted','gating');",
            )?;
            tx.pragma_update(None, "user_version", 185)?;
            tx.commit()?;
        }
        Ok(())
    }
}
