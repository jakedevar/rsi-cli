impl Store {
    fn migrate_v042(&self, version: i32) -> Result<()> {
        // V42: Lead/orchestrator pointer on Group/Epic container rows.
        // Points at a leaf child (parent_id == self.id) auto-promoted to lead.
        // Index supports rotation lead-inherit lookup (find_epics_by_lead).
        if version < 42 {
            self.add_column_if_not_exists("sessions", "lead_session_id", "TEXT")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_lead_session_id ON sessions(lead_session_id)",
                [],
            )?;
            tracing::info!("V42 migration complete: lead_session_id column + index");
            self.conn.pragma_update(None, "user_version", 42)?;
        }

        Ok(())
    }
}
