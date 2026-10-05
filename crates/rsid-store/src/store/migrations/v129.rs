impl Store {
    fn migrate_v129(&self, version: i32) -> Result<()> {
        // V129: durable topology executions, node attempts and events (#634).
        // Additive only.
        if version < 129 {
            topology_v129::apply_v129_migration(self)?;
        }

        Ok(())
    }
}
