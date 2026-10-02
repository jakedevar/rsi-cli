impl Store {
    fn migrate_v138(&self, version: i32) -> Result<()> {
        // Stable recursive manager principals and legacy root backfill.
        // The provisional version is assigned at landing.
        if version < 138 {
            manager_nodes::apply_manager_node_migration(self)?;
        }

        Ok(())
    }
}
