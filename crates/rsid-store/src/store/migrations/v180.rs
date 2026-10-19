// rsi-migration: contract
/// #1572: a deploy's request identity is separate from its current seat owner.
/// `owner_session_id` is the session that settles the deploy and moves with a
/// seat handover; the new immutable `request_session_id` is the session whose
/// idempotency-key namespace the request was made in. `UNIQUE(owner,
/// digest)` forced a handover to skip (or refuse) a live deploy whose key the
/// successor had already used; the unique key now sits on the request
/// identity, so the owner can change freely. Existing rows take their owner as
/// request identity. Operator-origin rows (no owner) have none. No row is
/// dropped or rewritten otherwise; the table is rebuilt only to replace the
/// unique constraint.
impl Store {
    fn migrate_v180(&self, version: i32) -> Result<()> {
        if version < 180 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE agent_deploys_rebuilt (
                    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
                    owner_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT CHECK(owner_session_id IS NULL OR rsi_uuid_is_canonical(owner_session_id)),
                    idempotency_digest TEXT NOT NULL CHECK(length(idempotency_digest)=64),
                    request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=64),
                    sha TEXT NOT NULL CHECK(length(sha)=40 AND sha=lower(sha) AND sha NOT GLOB '*[^0-9a-f]*'),
                    manifest_json TEXT NOT NULL CHECK(json_valid(manifest_json)),
                    state TEXT NOT NULL CHECK(state IN ('staged','restarting','succeeded','failed','timed_out')),
                    reason TEXT CHECK(reason IS NULL OR length(reason) BETWEEN 1 AND 1024),
                    wake_job_id TEXT UNIQUE CHECK(wake_job_id IS NULL OR rsi_uuid_is_canonical(wake_job_id)),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    deadline_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(deadline_at)),
                    restarted_at TEXT CHECK(restarted_at IS NULL OR rsi_rfc3339_nanos_is_canonical(restarted_at)),
                    settled_at TEXT CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
                    origin TEXT NOT NULL DEFAULT 'agent' CHECK(origin IN ('agent','operator')),
                    forced INTEGER NOT NULL DEFAULT 0 CHECK(forced IN (0,1)),
                    interrupt_workers INTEGER NOT NULL DEFAULT 0 CHECK(interrupt_workers IN (0,1)),
                    interrupted_sessions TEXT CHECK(interrupted_sessions IS NULL OR json_valid(interrupted_sessions)),
                    request_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT CHECK(request_session_id IS NULL OR rsi_uuid_is_canonical(request_session_id)),
                    UNIQUE(request_session_id, idempotency_digest),
                    CHECK((origin='agent' AND owner_session_id IS NOT NULL AND request_session_id IS NOT NULL) OR (origin='operator' AND owner_session_id IS NULL AND request_session_id IS NULL)),
                    CHECK((state IN ('staged','restarting') AND settled_at IS NULL AND wake_job_id IS NULL)
                       OR (state IN ('succeeded','failed','timed_out') AND settled_at IS NOT NULL
                           AND ((origin='agent' AND wake_job_id IS NOT NULL) OR (origin='operator' AND wake_job_id IS NULL))))
                );
                INSERT INTO agent_deploys_rebuilt (id, owner_session_id, idempotency_digest, request_fingerprint, sha,
                    manifest_json, state, reason, wake_job_id, created_at, deadline_at, restarted_at, settled_at,
                    origin, forced, interrupt_workers, interrupted_sessions, request_session_id)
                    SELECT id, owner_session_id, idempotency_digest, request_fingerprint, sha,
                    manifest_json, state, reason, wake_job_id, created_at, deadline_at, restarted_at, settled_at,
                    origin, forced, interrupt_workers, interrupted_sessions, owner_session_id
                    FROM agent_deploys;
                DROP TABLE agent_deploys;
                ALTER TABLE agent_deploys_rebuilt RENAME TO agent_deploys;
                CREATE UNIQUE INDEX agent_deploys_one_live ON agent_deploys((1)) WHERE state IN ('staged','restarting');
                CREATE TRIGGER agent_deploys_no_delete BEFORE DELETE ON agent_deploys BEGIN SELECT RAISE(ABORT,'agent deploys are append only'); END;
                CREATE TRIGGER agent_deploys_terminal_immutable BEFORE UPDATE ON agent_deploys WHEN OLD.state IN ('succeeded','failed','timed_out') BEGIN SELECT RAISE(ABORT,'settled agent deploys are immutable'); END;
                CREATE TRIGGER agent_deploys_request_identity_immutable BEFORE UPDATE OF request_session_id ON agent_deploys WHEN OLD.request_session_id IS NOT NEW.request_session_id BEGIN SELECT RAISE(ABORT,'agent deploy request identity is immutable'); END;",
            )?;
            tx.pragma_update(None, "user_version", 180)?;
            tx.commit()?;
        }
        Ok(())
    }
}
