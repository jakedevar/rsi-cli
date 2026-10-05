impl Store {
    fn migrate_v102(&self, version: i32) -> Result<()> {
        // V102: operator-appointed project manager and durable, correlated inbox.
        // Provider transport remains owned by existing terminal watches.
        if version < 102 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE harness_manager_scopes (
                    project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    epic_ids_json TEXT NOT NULL CHECK(json_valid(epic_ids_json)
                        AND json_type(epic_ids_json)='array' AND json_array_length(epic_ids_json)<=32),
                    row_version INTEGER NOT NULL CHECK(row_version>0),
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE harness_manager_messages (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    id TEXT NOT NULL UNIQUE,
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    epic_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    sender_session_id TEXT NOT NULL REFERENCES sessions(id),
                    recipient_session_id TEXT NOT NULL REFERENCES sessions(id),
                    request_id TEXT REFERENCES harness_manager_messages(id),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 128),
                    request_fingerprint TEXT NOT NULL,
                    message TEXT NOT NULL CHECK(length(CAST(message AS BLOB)) BETWEEN 1 AND 8192),
                    created_at TEXT NOT NULL,
                    UNIQUE(sender_session_id,idempotency_key)
                 );
                 CREATE INDEX harness_manager_messages_inbox
                    ON harness_manager_messages(project_id,scope_version,sequence);
                 CREATE INDEX harness_manager_messages_replies
                    ON harness_manager_messages(request_id,sequence);
                 CREATE TRIGGER harness_manager_messages_no_update
                    BEFORE UPDATE ON harness_manager_messages
                    BEGIN SELECT RAISE(ABORT,'manager messages are immutable'); END;
                 CREATE TRIGGER harness_manager_messages_no_delete
                    BEFORE DELETE ON harness_manager_messages
                    BEGIN SELECT RAISE(ABORT,'manager messages are retained for audit'); END;
                 CREATE TABLE harness_manager_watches (
                    job_id TEXT PRIMARY KEY NOT NULL REFERENCES scheduled_jobs(id),
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    epic_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    direction TEXT NOT NULL CHECK(direction IN ('to_manager','to_lead')),
                    source_session_id TEXT NOT NULL REFERENCES sessions(id),
                    target_session_id TEXT NOT NULL REFERENCES sessions(id),
                    attention_signature TEXT NOT NULL
                 );
                 CREATE INDEX harness_manager_watches_scope
                    ON harness_manager_watches(project_id,scope_version);",
            )?;
            tx.execute("PRAGMA user_version = 102", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
