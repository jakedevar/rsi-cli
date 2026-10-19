/// Satellite project scope (#1928): a declared scope entry may name a project
/// whose current appointed manager is the target, resolved live by the host
/// that owns the sessions, instead of a pinned session id. The hub stores the
/// declared project ids per peer; a satellite stores the ones its operator
/// declares for inbound hub traffic. Both are plain operator configuration,
/// replaced as a whole like the existing session scope tables. A queued hub
/// message that names the project authorizing its target (a session in the
/// project seat's tree) keeps that project in an append-only side table keyed
/// by message id, so the hub re-checks it on every delivery attempt.
impl Store {
    fn migrate_v166(&self, version: i32) -> Result<()> {
        if version < 166 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE satellite_peer_project_scope (
                    peer_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(peer_id)) REFERENCES satellite_peers(id) ON DELETE RESTRICT,
                    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    PRIMARY KEY(peer_id, project_id)
                );
                CREATE TABLE satellite_inbound_project_scope (
                    project_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(project_id)),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at))
                );
                CREATE TABLE satellite_message_scope_project (
                    message_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(message_id)) REFERENCES satellite_messages(id) ON DELETE RESTRICT,
                    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id))
                );
                CREATE TRIGGER satellite_message_scope_project_no_update BEFORE UPDATE ON satellite_message_scope_project BEGIN SELECT RAISE(ABORT,'satellite message scope is immutable'); END;
                CREATE TRIGGER satellite_message_scope_project_no_delete BEFORE DELETE ON satellite_message_scope_project BEGIN SELECT RAISE(ABORT,'satellite message scope is append only'); END;",
            )?;
            tx.pragma_update(None, "user_version", 166)?;
            tx.commit()?;
        }
        Ok(())
    }
}
