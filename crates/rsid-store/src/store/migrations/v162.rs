/// Durable, occurrence-bound operator answers from RSI Remote (#1695).
impl Store {
    fn migrate_v162(&self, version: i32) -> Result<()> {
        if version < 162 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE remote_answer_deliveries (
                    idempotency_key TEXT PRIMARY KEY NOT NULL CHECK(rsi_uuid_is_canonical(idempotency_key)),
                    request_fingerprint TEXT NOT NULL,
                    request_json TEXT NOT NULL CHECK(json_valid(request_json)),
                    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)) REFERENCES projects(id) ON DELETE RESTRICT,
                    session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(session_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
                    decision_id TEXT NOT NULL,
                    target_kind TEXT NOT NULL CHECK(target_kind IN ('question','appserver_approval')),
                    target_json TEXT NOT NULL CHECK(json_valid(target_json)),
                    target_digest TEXT NOT NULL,
                    answer TEXT NOT NULL CHECK(length(CAST(answer AS BLOB)) BETWEEN 1 AND 2048 AND instr(answer,char(0))=0),
                    origin_json TEXT NOT NULL CHECK(json_valid(origin_json) AND json_extract(origin_json,'$.kind')='remote'),
                    state TEXT NOT NULL CHECK(state IN ('queued','running','succeeded','refused','failed','uncertain')),
                    effect_started INTEGER NOT NULL DEFAULT 0 CHECK(effect_started IN (0,1)),
                    claim_boot_id TEXT CHECK(claim_boot_id IS NULL OR rsi_uuid_is_canonical(claim_boot_id)),
                    outcome_json TEXT NOT NULL CHECK(json_valid(outcome_json)),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
                    UNIQUE(session_id,decision_id,target_digest)
                );
                CREATE INDEX remote_answer_deliveries_pending
                    ON remote_answer_deliveries(created_at,idempotency_key) WHERE state IN ('queued','running');
                CREATE TRIGGER remote_answer_deliveries_immutable
                    BEFORE UPDATE ON remote_answer_deliveries
                    WHEN NEW.idempotency_key IS NOT OLD.idempotency_key
                        OR NEW.request_fingerprint IS NOT OLD.request_fingerprint
                        OR NEW.request_json IS NOT OLD.request_json
                        OR NEW.project_id IS NOT OLD.project_id
                        OR NEW.session_id IS NOT OLD.session_id
                        OR NEW.decision_id IS NOT OLD.decision_id
                        OR NEW.target_kind IS NOT OLD.target_kind
                        OR NEW.target_json IS NOT OLD.target_json
                        OR NEW.target_digest IS NOT OLD.target_digest
                        OR NEW.answer IS NOT OLD.answer
                        OR NEW.origin_json IS NOT OLD.origin_json
                        OR NEW.created_at IS NOT OLD.created_at
                    BEGIN SELECT RAISE(ABORT,'remote answer identity is immutable'); END;
                CREATE TRIGGER remote_answer_deliveries_forward
                    BEFORE UPDATE ON remote_answer_deliveries
                    WHEN OLD.state IN ('succeeded','refused','failed','uncertain')
                        OR NEW.effect_started<OLD.effect_started
                        OR (NEW.state='queued' AND (OLD.effect_started=1 OR NEW.effect_started=1))
                    BEGIN SELECT RAISE(ABORT,'remote answer transition is not forward'); END;",
            )?;
            tx.pragma_update(None, "user_version", 162)?;
            tx.commit()?;
        }
        Ok(())
    }
}
