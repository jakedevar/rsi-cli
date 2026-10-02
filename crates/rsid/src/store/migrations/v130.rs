impl Store {
    fn migrate_v130(&self, version: i32) -> Result<()> {
        // V130: append-only exact-request journal for agent child fresh relaunch.
        if version < 130 {
            agent_child_relaunch_intents::apply_v130_migration(self)?;
        }

        Ok(())
    }
}
