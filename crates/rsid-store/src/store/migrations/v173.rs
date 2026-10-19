/// Audited reproof of retained intents with no prior effect (#2134).
impl Store {
    fn migrate_v173(&self, version: i32) -> Result<()> {
        if version < 173 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            cohort_settlement::untouched_intent::install(&tx)?;
            tx.pragma_update(None, "user_version", 173)?;
            tx.commit()?;
        }
        Ok(())
    }
}
