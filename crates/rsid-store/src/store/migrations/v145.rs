impl Store {
    fn migrate_v145(&self, version: i32) -> Result<()> {
        // Issue #1017 slice 3: hub-to-satellite queued delivery. The version
        // is provisional; the lander assigns the final number.
        if version < 145 {
            satellite_dispatch::apply_migration(self, 145)?;
        }

        Ok(())
    }
}
