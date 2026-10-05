impl Store {
    fn migrate_v131(&self, version: i32) -> Result<()> {
        // V131: durable absent-root sandbox reclaim journal (Epic R R-a1).
        // RSI-RELEASED-MIGRATION-BEGIN: v131-sandbox-reclaim-journal-driver
        if version < 131 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            sandbox_reclaim::apply_sandbox_reclaim_journal_migration(&tx)?;
            tx.execute("PRAGMA user_version = 131", [])?;
            tx.commit()?;
        }
        // RSI-RELEASED-MIGRATION-END: v131-sandbox-reclaim-journal-driver

        Ok(())
    }
}
