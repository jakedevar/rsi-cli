impl Store {
    fn migrate_v120(&self, version: i32) -> Result<()> {
        // V120: bounded source-worktree batch/dependency substrate. This is
        // additive; V1 settlement receipts and all released V119 catalog
        // objects remain historical inputs.
        if version < 120 {
            self.apply_source_worktree_v120_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v120-source-worktree-batch-driver
    fn apply_source_worktree_v120_migration(&self) -> Result<()> {
        source_worktree_v120::apply_v120_migration(self)
    }
    // RSI-RELEASED-MIGRATION-END: v120-source-worktree-batch-driver
}
