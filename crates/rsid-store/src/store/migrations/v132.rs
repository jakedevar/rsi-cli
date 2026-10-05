impl Store {
    fn migrate_v132(&self, version: i32) -> Result<()> {
        // V132: persist operator model/effort selections for the next turn.
        if version < 132 {
            session_model_updates::apply_v132_migration(self)?;
        }

        Ok(())
    }
}
