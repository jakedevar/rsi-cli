/// Replacement guard for the append-only Issue handoff records (#1750
/// fix-forward). V164 refuses UPDATE and DELETE, but `INSERT OR REPLACE` /
/// `REPLACE` deletes the conflicting row without firing the DELETE trigger
/// while `recursive_triggers` is off, so a replacement write could change or
/// remove a retained record. A BEFORE INSERT guard refuses any insert that
/// collides with a retained row on either unique key: `(issue_id, sequence)`
/// or `(author_session_id, idempotency_key)` (the V157 pattern, #1396). The
/// guard fires before the conflict is resolved, so UPSERT is refused too.
impl Store {
    fn migrate_v165(&self, version: i32) -> Result<()> {
        if version < 165 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TRIGGER issue_handoffs_no_replace
                    BEFORE INSERT ON issue_handoffs
                    WHEN EXISTS(SELECT 1 FROM issue_handoffs
                        WHERE (issue_id=NEW.issue_id AND sequence=NEW.sequence)
                           OR (author_session_id=NEW.author_session_id
                               AND idempotency_key=NEW.idempotency_key))
                    BEGIN SELECT RAISE(ABORT,'issue handoffs are never replaced'); END;",
            )?;
            tx.pragma_update(None, "user_version", 165)?;
            tx.commit()?;
        }
        Ok(())
    }
}
