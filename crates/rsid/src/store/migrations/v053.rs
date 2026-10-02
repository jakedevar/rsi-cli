impl Store {
    fn migrate_v053(&self, version: i32) -> Result<()> {
        // V53: Recursive DAG scheduler leases.
        //
        // This is Phase 5A.3's per-graph scheduler lease foundation. It adds
        // durable lease metadata, expires any pre-lease active runs as stale
        // crash leftovers, and enforces at most one active scheduler run per
        // graph. Global concurrency caps are enforced by the store API because
        // they are policy, not a fixed schema invariant.
        if version < 53 {
            self.add_column_if_not_exists("recursive_scheduler_runs", "lease_owner", "TEXT")?;
            self.add_column_if_not_exists("recursive_scheduler_runs", "lease_token", "TEXT")?;
            self.add_column_if_not_exists(
                "recursive_scheduler_runs",
                "lease_heartbeat_at",
                "TEXT",
            )?;
            self.add_column_if_not_exists("recursive_scheduler_runs", "lease_expires_at", "TEXT")?;

            let migrated_at =
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            self.conn.execute(
                "UPDATE recursive_scheduler_runs
                 SET status = 'lease_expired',
                     completed_at = COALESCE(completed_at, ?1),
                     stop_reason = 'lease_expired',
                     failure_reason = COALESCE(
                         failure_reason,
                         'recursive scheduler active run expired during V53 lease migration'
                     )
                 WHERE status IN ('running', 'cancelling')",
                params![migrated_at],
            )?;

            self.conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_one_active_per_graph
                    ON recursive_scheduler_runs(graph_id)
                    WHERE status IN ('running', 'cancelling');
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_active_lease
                    ON recursive_scheduler_runs(status, lease_expires_at, started_at)
                    WHERE status IN ('running', 'cancelling');
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_lease_owner
                    ON recursive_scheduler_runs(lease_owner, status, lease_expires_at)
                    WHERE lease_owner IS NOT NULL;",
            )?;
            tracing::info!("V53 migration complete: recursive DAG scheduler leases");
            self.conn.pragma_update(None, "user_version", 53)?;
        }

        Ok(())
    }
}
