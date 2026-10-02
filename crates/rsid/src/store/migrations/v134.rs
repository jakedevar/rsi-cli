impl Store {
    fn migrate_v134(&self, version: i32) -> Result<()> {
        // V134: exact graceful-restart continuation owner.
        if version < 134 {
            restart_intents::apply_v134_migration(self)?;
        }

        Ok(())
    }
}
