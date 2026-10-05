impl Store {
    fn migrate_v125(&self, version: i32) -> Result<()> {
        // V125: rebuild the V97 `issue_events` audit table so an
        // IssueCoordinate manager mutation is recorded with a truthful
        // `manager` actor and no owning Epic (Issue #639). The literal must
        // equal issues::manager_actor_migration::MANAGER_ACTOR_VERSION.
        if version < 125 {
            issues::manager_actor_migration::apply_manager_actor_migration(self)?;
        }

        Ok(())
    }
}
