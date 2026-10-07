/// Issue #1604: queue source identity stays fixed while its pending outcome
/// follows manager seat succession. NULL retains the original source target,
/// including on existing immutable terminal rows.
impl Store {
    fn migrate_v160(&self, version: i32) -> Result<()> {
        if version < 160 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            add_column_if_not_exists_tx(
                &tx,
                "rolling_queue_entries",
                "wake_session_id",
                "TEXT CHECK(wake_session_id IS NULL OR rsi_uuid_is_canonical(wake_session_id))",
            )?;
            tx.pragma_update(None, "user_version", 160)?;
            tx.commit()?;
        }
        Ok(())
    }
}
