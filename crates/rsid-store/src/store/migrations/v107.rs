impl Store {
    fn migrate_v107(&self, version: i32) -> Result<()> {
        // V107: independent native approval occurrences. V106 remains intact as
        // historical evidence; migration cannot recover a live writer lease.
        // Request closure is separate from answer enqueue/consumption evidence.
        if version < 107 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE appserver_approval_publications (
                    publication_id TEXT PRIMARY KEY NOT NULL,
                    session_id TEXT NOT NULL REFERENCES sessions(id),
                    incarnation_id TEXT NOT NULL,
                    request_id_json TEXT NOT NULL CHECK(json_valid(request_id_json)),
                    thread_id TEXT,
                    approval_id TEXT NOT NULL REFERENCES approvals(id),
                    state TEXT NOT NULL CHECK(state IN ('unresolved','published','enqueued','expired','superseded')),
                    closure_state TEXT NOT NULL DEFAULT 'open' CHECK(closure_state IN ('open','closed','ambiguous')),
                    target_json TEXT NOT NULL CHECK(json_valid(target_json)),
                    closure_json TEXT CHECK(closure_json IS NULL OR json_valid(closure_json)),
                    outcome TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    closed_at TEXT
                 );
                 CREATE INDEX appserver_approval_publications_session
                    ON appserver_approval_publications(session_id,publication_id);
                 CREATE INDEX appserver_approval_publications_request
                    ON appserver_approval_publications(session_id,incarnation_id,request_id_json,created_at);
                 CREATE UNIQUE INDEX appserver_approval_publications_current
                    ON appserver_approval_publications(session_id,incarnation_id,request_id_json)
                    WHERE state IN ('unresolved','published','enqueued') AND closure_state <> 'closed';
                 CREATE INDEX appserver_approval_publications_live
                    ON appserver_approval_publications(state,closure_state,updated_at);
                 INSERT INTO appserver_approval_publications
                    (publication_id,session_id,incarnation_id,request_id_json,thread_id,
                     approval_id,state,closure_state,target_json,outcome,created_at,updated_at)
                    SELECT publication_id,session_id,incarnation_id,
                        COALESCE(target_json -> '$.request_id','null'),
                        json_extract(target_json,'$.params.threadId'),approval_id,'expired','open',
                        target_json,'V106 historical publication; live writer unavailable; prior state: ' || state,
                        updated_at,updated_at
                    FROM pending_appserver_approvals;",
            )?;
            tx.execute("PRAGMA user_version = 107", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
