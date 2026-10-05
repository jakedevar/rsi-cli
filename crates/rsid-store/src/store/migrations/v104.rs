impl Store {
    fn migrate_v104(&self, version: i32) -> Result<()> {
        // V104: explicit manager capabilities, typed coordination records and
        // durable action publication. V1 appointments receive no implicit grant.
        if version < 104 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE harness_manager_v2_policies (
                    project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    row_version INTEGER NOT NULL CHECK(row_version>0),
                    policy_json TEXT NOT NULL CHECK(json_valid(policy_json) AND length(policy_json)<=32768),
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE harness_manager_v2_operations (
                    id TEXT PRIMARY KEY NOT NULL,
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    policy_version INTEGER NOT NULL CHECK(policy_version>=0),
                    actor_session_id TEXT REFERENCES sessions(id),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 128),
                    fingerprint TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(payload_json)<=65536),
                    state TEXT NOT NULL CHECK(state IN ('queued','running','succeeded','failed','blocked','uncertain','revoked')),
                    row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version>0),
                    target_session_id TEXT,
                    outcome_json TEXT CHECK(outcome_json IS NULL OR json_valid(outcome_json)),
                    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts>=0),
                    not_before TEXT NOT NULL,
                    claim_boot_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE(project_id,manager_session_id,scope_version,idempotency_key)
                 );
                 CREATE INDEX harness_manager_v2_operation_due
                    ON harness_manager_v2_operations(state,not_before,id);
                 CREATE INDEX harness_manager_v2_operation_scope
                    ON harness_manager_v2_operations(project_id,manager_session_id,scope_version,state,id);
                 CREATE TABLE harness_manager_v2_records (
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    kind TEXT NOT NULL,
                    record_key TEXT NOT NULL CHECK(length(CAST(record_key AS BLOB)) BETWEEN 1 AND 256),
                    epic_id TEXT REFERENCES sessions(id),
                    row_version INTEGER NOT NULL CHECK(row_version>0),
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(payload_json)<=65536),
                    archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY(project_id,manager_session_id,scope_version,kind,record_key)
                 );
                 CREATE INDEX harness_manager_v2_record_epic
                    ON harness_manager_v2_records(project_id,scope_version,epic_id,kind,record_key);
                 CREATE TABLE harness_manager_v2_events (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    actor_session_id TEXT REFERENCES sessions(id),
                    kind TEXT NOT NULL,
                    record_key TEXT NOT NULL,
                    row_version INTEGER NOT NULL CHECK(row_version>=0),
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(payload_json)<=65536),
                    created_at TEXT NOT NULL
                 );
                 CREATE INDEX harness_manager_v2_event_scope
                    ON harness_manager_v2_events(project_id,manager_session_id,scope_version,sequence);
                 CREATE TRIGGER harness_manager_v2_events_no_update
                    BEFORE UPDATE ON harness_manager_v2_events
                    BEGIN SELECT RAISE(ABORT,'immutable manager event'); END;
                 CREATE TRIGGER harness_manager_v2_events_no_delete
                    BEFORE DELETE ON harness_manager_v2_events
                    BEGIN SELECT RAISE(ABORT,'immutable manager event'); END;
                 CREATE TABLE harness_manager_v2_entities (
                    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id),
                    operation_id TEXT NOT NULL UNIQUE REFERENCES harness_manager_v2_operations(id),
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    policy_version INTEGER NOT NULL CHECK(policy_version>0),
                    kind TEXT NOT NULL,
                    created_at TEXT NOT NULL
                 );
                 CREATE INDEX harness_manager_v2_entity_scope
                    ON harness_manager_v2_entities(project_id,manager_session_id,scope_version,kind);",
            )?;
            tx.execute("PRAGMA user_version = 104", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
