impl Store {
    fn migrate_v148(&self, version: i32) -> Result<()> {
        // Issue #794 S1: durable Harness completion gates. The version is
        // provisional; the lander assigns the final number.
        if version < 148 {
            session_completion_gates::apply_migration(self, 148)?;
        }

        Ok(())
    }
}
