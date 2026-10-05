impl Store {
    fn migrate_v064(&self, version: i32) -> Result<()> {
        // V64: Recursive recovery pass idempotency and topology provenance.
        //
        // T5 keeps topology-scoped recovery as a selector-bound wrapper around
        // the existing recursive recovery pass tables. These columns let the
        // wrapper safely replay caller-keyed requests and expose audit context
        // without introducing a topology-specific recovery table.
        if version < 64 {
            self.add_column_if_not_exists("recursive_recovery_passes", "idempotency_key", "TEXT")?;
            self.add_column_if_not_exists(
                "recursive_recovery_passes",
                "request_fingerprint",
                "TEXT",
            )?;
            self.add_column_if_not_exists(
                "recursive_recovery_passes",
                "source_context_json",
                "TEXT CHECK (source_context_json IS NULL OR json_valid(source_context_json))",
            )?;
            self.conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_recovery_pass_idempotency_key_unique
                    ON recursive_recovery_passes(idempotency_key)
                    WHERE idempotency_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_source_context_started
                    ON recursive_recovery_passes(source, started_at DESC, id DESC)
                    WHERE source_context_json IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_recovery_topology
                    ON recursive_topology_graph_links(
                        execution_owner, topology_id, source_topology_iteration,
                        source_topology_node_id, created_at, graph_id
                    );
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_recovery_workflow_execution
                    ON recursive_topology_graph_links(
                        execution_owner, workflow_execution_id, source_topology_iteration,
                        source_topology_node_id, created_at, graph_id
                    )
                    WHERE workflow_execution_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_recovery_node
                    ON recursive_topology_graph_links(
                        execution_owner, topology_id, source_topology_node_id,
                        source_topology_iteration, created_at, graph_id
                    );",
            )?;
            tracing::info!(
                "V64 migration complete: recursive recovery idempotency and topology indexes"
            );
            self.conn.pragma_update(None, "user_version", 64)?;
        }

        Ok(())
    }
}
