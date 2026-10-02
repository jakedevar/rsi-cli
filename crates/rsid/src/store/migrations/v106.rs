impl Store {
    fn migrate_v106(&self, version: i32) -> Result<()> {
        // V106: exact native AppServer approval publication. Existing approvals
        // remain displayable, but receive no inferred provider reply identity.
        if version < 106 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE pending_appserver_approvals (
                    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id),
                    publication_id TEXT NOT NULL UNIQUE,
                    incarnation_id TEXT NOT NULL,
                    approval_id TEXT NOT NULL REFERENCES approvals(id),
                    state TEXT NOT NULL CHECK(state IN ('unresolved','published','enqueued','expired')),
                    target_json TEXT NOT NULL CHECK(json_valid(target_json)),
                    outcome TEXT,
                    updated_at TEXT NOT NULL
                 );
                 CREATE INDEX pending_appserver_approvals_live ON pending_appserver_approvals(state,updated_at);",
            )?;
            tx.execute("PRAGMA user_version = 106", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
