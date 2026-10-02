impl Store {
    fn migrate_v143(&self, version: i32) -> Result<()> {
        // Issue #792: per-session Harness tool policy. The version is
        // provisional; the lander assigns the final number.
        if version < 143 {
            session_tool_policy::apply_migration(self, 143)?;
        }

        Ok(())
    }
}
