// rsi-migration: additive
/// #1647 P1 slice 3: durable ownership of a shim invocation across daemon boots.
impl Store {
    fn migrate_v161(&self, version: i32) -> Result<()> {
        if version < 161 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE provider_turn_custody (
                    invocation_id TEXT PRIMARY KEY NOT NULL CHECK(rsi_uuid_is_canonical(invocation_id)) REFERENCES model_invocations(id) ON DELETE RESTRICT,
                    session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(session_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
                    spool_dir TEXT NOT NULL UNIQUE CHECK(length(spool_dir)>0),
                    pid INTEGER NOT NULL CHECK(typeof(pid)='integer' AND pid>0 AND pid<=2147483647),
                    start_time INTEGER CHECK(start_time IS NULL OR (typeof(start_time)='integer' AND start_time>=0)),
                    boot_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(boot_id)),
                    stdout_offset INTEGER NOT NULL DEFAULT 0 CHECK(typeof(stdout_offset)='integer' AND stdout_offset>=0),
                    state TEXT NOT NULL CHECK(state IN ('live','adopted','finished','abandoned')),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
                );
                CREATE UNIQUE INDEX provider_turn_custody_active_session
                    ON provider_turn_custody(session_id) WHERE state IN ('live','adopted');
                CREATE TRIGGER provider_turn_custody_invocation_session
                    BEFORE INSERT ON provider_turn_custody
                    WHEN NOT EXISTS(SELECT 1 FROM model_invocations
                        WHERE id=NEW.invocation_id AND session_id=NEW.session_id)
                    BEGIN SELECT RAISE(ABORT,'turn custody invocation belongs to another session'); END;
                CREATE TRIGGER provider_turn_custody_identity_immutable
                    BEFORE UPDATE ON provider_turn_custody
                    WHEN NEW.invocation_id IS NOT OLD.invocation_id
                        OR NEW.session_id IS NOT OLD.session_id
                        OR NEW.spool_dir IS NOT OLD.spool_dir
                        OR NEW.pid IS NOT OLD.pid
                        OR NEW.start_time IS NOT OLD.start_time
                        OR NEW.created_at IS NOT OLD.created_at
                    BEGIN SELECT RAISE(ABORT,'turn custody identity is immutable'); END;
                CREATE TRIGGER provider_turn_custody_forward
                    BEFORE UPDATE ON provider_turn_custody
                    WHEN OLD.state IN ('finished','abandoned')
                        OR NEW.stdout_offset<OLD.stdout_offset
                        OR (OLD.state='adopted' AND NEW.state='live')
                        OR (NEW.boot_id!=OLD.boot_id AND NEW.state!='adopted')
                    BEGIN SELECT RAISE(ABORT,'turn custody transition is not forward'); END;",
            )?;
            tx.pragma_update(None, "user_version", 161)?;
            tx.commit()?;
        }
        Ok(())
    }
}
