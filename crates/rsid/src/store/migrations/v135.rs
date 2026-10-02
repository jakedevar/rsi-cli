impl Store {
    fn migrate_v135(&self, version: i32) -> Result<()> {
        // V135: stable, locally persisted installation identity.
        if version < 135 {
            satellite_identity::apply_v135_migration(self)?;
        }

        Ok(())
    }
}
