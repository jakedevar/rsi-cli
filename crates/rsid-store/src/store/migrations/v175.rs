/// The Session dependency-health classifier, versioned (#2050): the evidence view
/// moves to the V175 relevance function and the health index is re-derived.
impl Store {
    fn migrate_v175(&self, version: i32) -> Result<()> {
        if version < 175 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            source_worktree_v120::health_v175::install(&tx)?;
            tx.pragma_update(None, "user_version", 175)?;
            tx.commit()?;
        }
        Ok(())
    }
}
