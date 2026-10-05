impl Store {
    fn migrate_v141(&self, version: i32) -> Result<()> {
        // Issue #1035: terminal target-reclaim evidence is keyed by the exact
        // target identity so a rebuilt target can be reclaimed again. The
        // version is provisional; the lander assigns the final number.
        if version < 141 {
            target_reclaim_sweep::apply_identity_events_migration(self, 141)?;
        }

        Ok(())
    }
}
