impl Store {
    fn migrate_v058(&self, version: i32) -> Result<()> {
        // V58: Recursive DAG live attempt heartbeat readback support.
        //
        // Phase 6.4A uses the owner/token/heartbeat columns introduced with
        // recursive_live_attempts in V55. This migration adds only an index for
        // token-owned active attempt expiry scans; it does not add recovery,
        // background heartbeat loops, or live scheduler reachability.
        if version < 58 {
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_heartbeat_expiry
                    ON recursive_live_attempts(status, lease_expires_at, heartbeat_at, id)
                    WHERE lease_token IS NOT NULL
                      AND heartbeat_at IS NOT NULL
                      AND lease_expires_at IS NOT NULL;",
            )?;
            tracing::info!("V58 migration complete: recursive DAG live attempt heartbeat index");
            self.conn.pragma_update(None, "user_version", 58)?;
        }

        Ok(())
    }
}
