impl Store {
    fn migrate_v116(&self, version: i32) -> Result<()> {
        // V116: preserve the V115 journal while allowing an authority-
        // preserving manager lineage tip to remain the attributed caller.
        if version < 116 {
            self.apply_manager_prepared_actions_v116_migration()?;
        }

        Ok(())
    }
}
