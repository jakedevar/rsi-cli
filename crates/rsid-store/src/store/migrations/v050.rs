impl Store {
    fn migrate_v050(&self, version: i32) -> Result<()> {
        // V50: Recursive DAG recovery quarantine metadata.
        //
        // Quarantined graphs remain inspectable through read models but are
        // excluded from normal recursive DAG write APIs. `recovered_at` existed
        // in V49; V50 adds explicit quarantine and last effective recovery
        // check evidence without broadening the mutation surface.
        if version < 50 {
            self.add_column_if_not_exists("recursive_task_graphs", "quarantined_at", "TEXT")?;
            self.add_column_if_not_exists("recursive_task_graphs", "quarantine_reason", "TEXT")?;
            self.add_column_if_not_exists("recursive_task_graphs", "recovery_checked_at", "TEXT")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_recursive_graph_quarantine
                    ON recursive_task_graphs(quarantined_at, updated_at DESC)
                    WHERE quarantined_at IS NOT NULL",
                [],
            )?;
            tracing::info!("V50 migration complete: recursive DAG quarantine metadata");
            self.conn.pragma_update(None, "user_version", 50)?;
        }

        Ok(())
    }
}
