impl Store {
    fn migrate_v047(&self, version: i32) -> Result<()> {
        if version < 47 {
            self.add_column_if_not_exists("sessions", "topology_node_id", "TEXT")?;
            self.add_column_if_not_exists(
                "sessions",
                "topology_iteration",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_topology_node \
                 ON sessions(parent_id, topology_node_id)",
                [],
            )?;
            tracing::info!("V47 migration complete: topology_node_id + topology_iteration columns");
            self.conn.pragma_update(None, "user_version", 47)?;
        }

        Ok(())
    }
}
