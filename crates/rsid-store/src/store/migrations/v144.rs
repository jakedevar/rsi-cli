impl Store {
    fn migrate_v144(&self, version: i32) -> Result<()> {
        // Issue #1002: daemon-owned durable agent jobs. The version is
        // provisional; the lander assigns the final number.
        if version < 144 {
            agent_jobs::apply_migration(self, 144)?;
        }

        Ok(())
    }
}
