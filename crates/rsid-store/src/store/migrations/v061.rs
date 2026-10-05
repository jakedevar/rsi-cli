impl Store {
    fn migrate_v061(&self, version: i32) -> Result<()> {
        // V61: Index read-only recursive DAG live validation status paths.
        //
        // Phase 6.5D exposes validation readback anchored by graph, task,
        // scheduler run, and output kind. These indexes are read-only support
        // only; no RPC handler creates indexes opportunistically at runtime.
        if version < 61 {
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_graph
                    ON recursive_live_output_validations(graph_id, created_at DESC, id);
                 CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_task
                    ON recursive_live_output_validations(graph_id, task_id, created_at DESC, id);
                 CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_run
                    ON recursive_live_output_validations(scheduler_run_id, created_at DESC, id);
                 CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_output_kind
                    ON recursive_live_output_validations(output_kind, created_at DESC, id)
                    WHERE output_kind IS NOT NULL;",
            )?;
            tracing::info!("V61 migration complete: recursive DAG live validation read indexes");
            self.conn.pragma_update(None, "user_version", 61)?;
        }

        Ok(())
    }
}
