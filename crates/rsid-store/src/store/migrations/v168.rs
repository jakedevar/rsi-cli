/// Admit Deleted owners to the retained settlement journal without resurrecting
/// their Sessions. Existing journal rows and all guards are preserved (#1909).
impl Store {
    fn migrate_v168(&self, version: i32) -> Result<()> {
        if version < 168 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            cohort_settlement::rebuild_settlement_status_constraint(&tx, true)?;
            tx.pragma_update(None, "user_version", 168)?;
            tx.commit()?;
        } else {
            let sql: String = self.conn.query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='source_worktree_settlement_items'",
                [],
                |row| row.get(0),
            )?;
            if !sql.contains(
                "original_status IN ('Completed','Failed','Interrupted','Archived','Deleted')",
            ) {
                return Err(DaemonError::Store(
                    "settlement Deleted-owner status constraint is missing".into(),
                ));
            }
        }
        Ok(())
    }
}
