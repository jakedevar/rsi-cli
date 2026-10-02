impl Store {
    fn migrate_v140(&self, version: i32) -> Result<()> {
        // Issue #1007: durable rolling merge queue. The version is provisional;
        // the lander assigns the final number.
        if version < 140 {
            rolling_queue::apply_migration(self, 140)?;
        }

        Ok(())
    }
}
