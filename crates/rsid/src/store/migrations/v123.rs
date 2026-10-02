impl Store {
    fn migrate_v123(&self, version: i32) -> Result<()> {
        // V123: bound terminal-watch restart repair's historical owner/child
        // lookup by its natural key while preserving every V122 catalog pin.
        if version < 123 {
            agent_coordination::watch_repair_v123::apply_v123_migration(self)?;
        }

        Ok(())
    }
}
