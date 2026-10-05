impl Store {
    fn migrate_v128(&self, version: i32) -> Result<()> {
        // V128: immutable, durable evidence of watchdog-triggered restarts.
        if version < 128 {
            crate::daemon_restart_persistence::apply_v128_migration(self)?;
        }

        Ok(())
    }
}
