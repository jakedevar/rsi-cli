impl Store {
    fn migrate_v059(&self, version: i32) -> Result<()> {
        // V59: Tighten recursive DAG live heartbeat stale-scan index.
        //
        // V58 introduced the heartbeat expiry index before the stale scan
        // required heartbeat_at to be present. Rebuild the partial index so
        // already-migrated V58 databases get the same narrower index shape as
        // fresh databases.
        if version < 59 {
            self.conn.execute_batch(
                "DROP INDEX IF EXISTS idx_recursive_live_attempts_heartbeat_expiry;
                 CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_heartbeat_expiry
                    ON recursive_live_attempts(status, lease_expires_at, heartbeat_at, id)
                    WHERE lease_token IS NOT NULL
                      AND heartbeat_at IS NOT NULL
                      AND lease_expires_at IS NOT NULL;",
            )?;
            tracing::info!("V59 migration complete: recursive DAG live heartbeat index tightened");
            self.conn.pragma_update(None, "user_version", 59)?;
        }

        Ok(())
    }
}
