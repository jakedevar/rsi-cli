impl Store {
    fn migrate_v146(&self, version: i32) -> Result<()> {
        // Issue #1045: daemon-owned agent deploys. The version is provisional;
        // the lander assigns the final number.
        if version < 146 {
            agent_deploys::apply_migration(self, 146)?;
        }

        Ok(())
    }
}
