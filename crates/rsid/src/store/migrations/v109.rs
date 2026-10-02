impl Store {
    fn migrate_v109(&self, version: i32) -> Result<()> {
        // V109: bounded legacy approval candidates and exact native mirror
        // exclusion. Both indexes preserve all historical approval evidence.
        if version < 109 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE INDEX manager_pending_approval_scan ON approvals(session_id,id) WHERE status='Pending';
                 CREATE INDEX manager_native_approval_mirror ON appserver_approval_publications(approval_id);",
            )?;
            tx.execute("PRAGMA user_version = 109", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
