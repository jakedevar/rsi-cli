impl Store {
    fn migrate_v153(&self, version: i32) -> Result<()> {
        if version < 153 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch("CREATE INDEX idx_fleet_invocations_created ON model_invocations(created_at DESC, id);
                CREATE INDEX idx_fleet_active_sessions ON sessions(updated_at DESC, id) WHERE status IN ('Starting','Running','WaitingApproval');
                CREATE INDEX idx_fleet_session_invocations ON model_invocations(session_id, status, created_at DESC);")?;
            tx.pragma_update(None, "user_version", 153)?;
            tx.commit()?;
        }
        Ok(())
    }
}
