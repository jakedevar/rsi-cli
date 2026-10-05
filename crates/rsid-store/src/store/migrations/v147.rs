impl Store {
    fn migrate_v147(&self, version: i32) -> Result<()> {
        // Issue #1059: satellite inbound write-ahead attempts. The version is
        // provisional; the lander assigns the final number.
        if version < 147 {
            satellite_inbound_attempts::apply_migration(self, 147)?;
        }

        Ok(())
    }
}
