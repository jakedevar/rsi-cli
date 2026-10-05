impl Store {
    fn migrate_v133(&self, version: i32) -> Result<()> {
        // V133: append-only agent topology request ledger (#633).
        if version < 133 {
            topology_agent_audit::apply_v133_migration(self)?;
        }

        Ok(())
    }
}
