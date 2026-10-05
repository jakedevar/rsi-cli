impl Store {
    fn migrate_v063(&self, version: i32) -> Result<()> {
        // V63: Recursive cancellation idempotency for topology wrappers.
        //
        // T4 keeps topology cancellation as a thin wrapper over recursive DAG
        // cancellation requests. These columns let the wrapper de-duplicate
        // concrete graph/run targets without changing generic cancellation RPC
        // behavior or adding a second cancellation table.
        if version < 63 {
            self.add_column_if_not_exists(
                "recursive_cancellation_requests",
                "idempotency_key",
                "TEXT",
            )?;
            self.add_column_if_not_exists(
                "recursive_cancellation_requests",
                "request_fingerprint",
                "TEXT",
            )?;
            self.add_column_if_not_exists(
                "recursive_cancellation_requests",
                "source_context_json",
                "TEXT CHECK (source_context_json IS NULL OR json_valid(source_context_json))",
            )?;
            self.conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_cancel_idempotency_key_unique
                    ON recursive_cancellation_requests(idempotency_key)
                    WHERE idempotency_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_graph_scope_status_requested
                    ON recursive_cancellation_requests(graph_id, scope, status, requested_at, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_run_scope_status_requested
                    ON recursive_cancellation_requests(run_id, scope, status, requested_at, id)
                    WHERE run_id IS NOT NULL;",
            )?;
            tracing::info!("V63 migration complete: recursive cancellation idempotency indexes");
            self.conn.pragma_update(None, "user_version", 63)?;
        }

        Ok(())
    }
}
