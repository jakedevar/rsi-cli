// rsi-migration: additive
/// #2364: startup absent-root adoption journals a retained candidate only when its
/// outcome differs from its previous journalled outcome. The lookup is by
/// `custody_id` (newest row first), which the `(run_id, custody_id)` key cannot serve.
impl Store {
    fn migrate_v174(&self, version: i32) -> Result<()> {
        if version < 174 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE INDEX sandbox_reclaim_items_custody ON sandbox_reclaim_items(custody_id);",
            )?;
            tx.pragma_update(None, "user_version", 174)?;
            tx.commit()?;
        }
        Ok(())
    }
}
