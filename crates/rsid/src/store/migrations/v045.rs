impl Store {
    fn migrate_v045(&self, version: i32) -> Result<()> {
        // V45: Capability class for routing enforcement (RSI-010).
        // Nullable TEXT column; pre-V45 rows deserialize to `None`.
        // (Originally written as V41 in PR #13; renumbered during 2026-05-13 cherry-pick replay
        // because V41–V44 were taken by main between PR #13 author time and replay.)
        if version < 45 {
            self.add_column_if_not_exists("sessions", "capability_class", "TEXT")?;
            tracing::info!("V45 migration complete: capability_class column (RSI-010)");
            self.conn.pragma_update(None, "user_version", 45)?;
        }

        Ok(())
    }
}
