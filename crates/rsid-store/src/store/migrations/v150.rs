impl Store {
    fn migrate_v150(&self, version: i32) -> Result<()> {
        // Issue #872 Slice B: global manager grants, messages and appointments.
        // The version is provisional; the lander assigns the final number.
        if version < 150 {
            global_manager::apply_migration(self, 150)?;
        }

        Ok(())
    }
}
