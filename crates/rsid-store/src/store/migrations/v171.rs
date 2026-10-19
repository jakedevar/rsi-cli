/// One audited, superseding reproof attempt for pre-removal quarantine fences (#2098).
impl Store {
    fn migrate_v171(&self, version: i32) -> Result<()> {
        if version < 171 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            cohort_settlement::reproof::install(&tx)?;
            tx.pragma_update(None, "user_version", 171)?;
            tx.commit()?;
        }
        Ok(())
    }
}
