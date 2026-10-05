// V136's released call used the then-current global head. Preserve its pinned
// block exactly, with that former value bound locally during replay.
#[rustfmt::skip]
impl Store {
    fn migrate_v136(&self, version: i32) -> Result<()> {
        const LATEST_SCHEMA_VERSION: i32 = 136;
        // Inert satellite peer/link registry; operator controls arrive in S3.
        if version < 136 {
            satellite_registry::apply_migration(self, LATEST_SCHEMA_VERSION)?;
        }
        Ok(())
    }
}
