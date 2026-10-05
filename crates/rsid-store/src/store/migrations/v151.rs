impl Store {
    fn migrate_v151(&self, version: i32) -> Result<()> {
        // Issue #1122: operator-requested quiet-point restarts reuse the deploy
        // catalog with no owning session. The version is provisional; the
        // lander assigns the final number.
        if version < 151 {
            agent_deploys_operator::apply_migration(self, 151)?;
        }

        Ok(())
    }
}
