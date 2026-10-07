impl Store {
    fn migrate_v155(&self, version: i32) -> Result<()> {
        // Issue #1238 (fractal manager hierarchy S4, M2): reports and
        // escalations route up N levels to the operator; down-mail reaches
        // any descendant. Tier messages, escalation hops above a project root
        // and their immutable events. The version is provisional; the lander
        // assigns the final number.
        if version < 155 {
            manager_tier_routing::apply_migration(self, 155)?;
        }

        Ok(())
    }
}
