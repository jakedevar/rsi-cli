impl Store {
    fn migrate_v137(&self, version: i32) -> Result<()> {
        // Durable migration allocation claims; the version is provisional.
        if version < 137 {
            migration_allocation::apply_migration(self, 137)?;
        }

        Ok(())
    }
}
