// rsi-migration: additive
/// #2021: a portfolio seat hands its own seat to a successor it launches. The
/// appointment row of such a self-handoff records where the predecessor wrote
/// its handoff (a repo path or a commit); the old seat, new seat and time are
/// its existing `caller_session_id`, `session_id` and `updated_at`.
impl Store {
    fn migrate_v170(&self, version: i32) -> Result<()> {
        if version < 170 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "ALTER TABLE manager_portfolio_appointments ADD COLUMN handoff TEXT
                    CHECK(handoff IS NULL OR (length(handoff) BETWEEN 1 AND 512));
                CREATE TRIGGER manager_portfolio_appointments_handoff_immutable
                    BEFORE UPDATE OF handoff ON manager_portfolio_appointments
                    WHEN NEW.handoff IS NOT OLD.handoff
                    BEGIN SELECT RAISE(ABORT,'a portfolio appointment handoff is immutable'); END;",
            )?;
            tx.pragma_update(None, "user_version", 170)?;
            tx.commit()?;
        }
        Ok(())
    }
}
