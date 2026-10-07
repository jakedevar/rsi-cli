impl Store {
    fn migrate_v154(&self, version: i32) -> Result<()> {
        // Issue #1236 (fractal manager hierarchy S2, M1): portfolio node
        // identity. Nodes, their coverage projection and the node-keyed
        // columns of `global_manager_grants`; an active v0 grant becomes one
        // root labelled `global`. The version is provisional; the lander
        // assigns the final number.
        if version < 154 {
            portfolio_nodes::apply_migration(self, 154)?;
        }

        Ok(())
    }
}
