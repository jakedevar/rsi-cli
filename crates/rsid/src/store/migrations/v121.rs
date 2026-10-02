impl Store {
    fn migrate_v121(&self, version: i32) -> Result<()> {
        // V121: exact-source manager review assignments and immutable,
        // daemon-attributed reviewer receipts. Legacy evidence stays readable.
        if version < 121 {
            self.apply_manager_review_v121_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v121-manager-review-driver
    fn apply_manager_review_v121_migration(&self) -> Result<()> {
        manager_review_v121::apply_v121_migration(self)
    }
    // RSI-RELEASED-MIGRATION-END: v121-manager-review-driver
}
