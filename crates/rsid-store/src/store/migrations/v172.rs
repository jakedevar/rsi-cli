/// Audited retries of interrupted settlement completion (#2111).
impl Store {
    fn migrate_v172(&self, version: i32) -> Result<()> {
        if version < 172 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            cohort_settlement::completion_retry::install(&tx)?;
            tx.pragma_update(None, "user_version", 172)?;
            tx.commit()?;
        }
        Ok(())
    }
}
