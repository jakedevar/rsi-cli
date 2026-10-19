/// Append-only Issue handoff records (#1750): a bound worker's complete
/// handoff is its own row, so a full Issue body never blocks publishing it.
impl Store {
    fn migrate_v164(&self, version: i32) -> Result<()> {
        if version < 164 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE issue_handoffs (
                    issue_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(issue_id)) REFERENCES issues(id) ON DELETE RESTRICT,
                    sequence INTEGER NOT NULL CHECK(sequence>=1),
                    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)),
                    author_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(author_session_id)),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 128 AND instr(idempotency_key,char(0))=0),
                    body TEXT NOT NULL CHECK(length(CAST(body AS BLOB)) BETWEEN 1 AND 65536 AND instr(body,char(0))=0),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    PRIMARY KEY(issue_id,sequence),
                    UNIQUE(author_session_id,idempotency_key)
                );
                CREATE TRIGGER issue_handoffs_no_update
                    BEFORE UPDATE ON issue_handoffs
                    BEGIN SELECT RAISE(ABORT,'issue handoffs are append-only'); END;
                CREATE TRIGGER issue_handoffs_no_delete
                    BEFORE DELETE ON issue_handoffs
                    BEGIN SELECT RAISE(ABORT,'issue handoffs are append-only'); END;",
            )?;
            tx.pragma_update(None, "user_version", 164)?;
            tx.commit()?;
        }
        Ok(())
    }
}
