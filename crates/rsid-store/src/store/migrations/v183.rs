// rsi-migration: additive
/// #3176: the durable exec target of a container-bound Harness session. One
/// immutable row per session, written once at launch (`ExecTarget::Container`
/// as JSON). A continuation, a resume after a daemon restart and a context
/// rotation resolve it through the `continued_from` chain and rebuild the same
/// target, or refuse: a container-bound session never falls back to the host.
/// Rows are append-only so the target a session ran against stays auditable.
impl Store {
    fn migrate_v183(&self, version: i32) -> Result<()> {
        if version < 183 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE session_exec_targets (
                    session_id TEXT PRIMARY KEY NOT NULL
                        REFERENCES sessions(id) ON DELETE RESTRICT
                        CHECK(rsi_uuid_is_canonical(session_id)),
                    target_json TEXT NOT NULL CHECK(json_valid(target_json)),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at))
                );
                CREATE TRIGGER session_exec_targets_no_update
                    BEFORE UPDATE ON session_exec_targets
                    BEGIN SELECT RAISE(ABORT,'session exec targets are immutable'); END;
                CREATE TRIGGER session_exec_targets_no_delete
                    BEFORE DELETE ON session_exec_targets
                    BEGIN SELECT RAISE(ABORT,'session exec targets are append only'); END;",
            )?;
            tx.pragma_update(None, "user_version", 183)?;
            tx.commit()?;
        }
        Ok(())
    }
}
