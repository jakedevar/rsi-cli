impl Store {
    fn migrate_v127(&self, version: i32) -> Result<()> {
        // V127: persist bounded session-attributed daemon diagnostics.
        if version < 127 {
            session_diagnostics::apply_v127_migration(self)?;
        }

        Ok(())
    }
}
