/// Issue #1239 (fractal manager hierarchy S5, M3): delegated appointments.
/// `manager_portfolio_appointments` generalizes V150's
/// `global_manager_appointments`: any portfolio node (the grantor) launches a
/// Standard root session and seats it as a child, a project's manager
/// (`project:<id>`) or a child portfolio node (`portfolio:<id>`). One row per
/// `(grantor node, idempotency key)`; rows are retained, their identity is
/// immutable and the state moves only `launched` to `appointed`. The
/// recorded epoch fences the appointment: an operator re-grant of the grantor
/// between the launch and the appointment refuses it.
// RSI-RELEASED-MIGRATION-BEGIN: v156-portfolio-appointments
const V156_PORTFOLIO_APPOINTMENTS: &str = "CREATE TABLE manager_portfolio_appointments (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    grantor_node_id TEXT NOT NULL REFERENCES manager_portfolio_nodes(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(grantor_node_id)),
    grantor_authority_epoch INTEGER NOT NULL CHECK(grantor_authority_epoch>0),
    target_ref TEXT NOT NULL CHECK(
        (length(target_ref)=44 AND substr(target_ref,1,8)='project:' AND rsi_uuid_is_canonical(substr(target_ref,9)))
     OR (length(target_ref)=46 AND substr(target_ref,1,10)='portfolio:' AND rsi_uuid_is_canonical(substr(target_ref,11)))),
    launch_project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(launch_project_id)),
    caller_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(caller_session_id)),
    idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 128),
    request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
    session_id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(session_id)),
    state TEXT NOT NULL CHECK(state IN ('launched','appointed')),
    scope_version INTEGER,
    policy_version INTEGER,
    policy_request_json TEXT CHECK(policy_request_json IS NULL OR (json_valid(policy_request_json) AND json_type(policy_request_json)='object')),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    reserved_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(reserved_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    UNIQUE(grantor_node_id, idempotency_key),
    CHECK((state='launched' AND scope_version IS NULL AND policy_version IS NULL)
       OR (state='appointed' AND scope_version IS NOT NULL AND policy_version IS NOT NULL
           AND scope_version>0 AND policy_version>0))
);
CREATE INDEX manager_portfolio_appointments_by_target ON manager_portfolio_appointments(target_ref, session_id);
CREATE TRIGGER manager_portfolio_appointments_no_delete BEFORE DELETE ON manager_portfolio_appointments
 BEGIN SELECT RAISE(ABORT,'portfolio appointments are retained'); END;
CREATE TRIGGER manager_portfolio_appointments_identity_immutable BEFORE UPDATE ON manager_portfolio_appointments
 WHEN NEW.id IS NOT OLD.id OR NEW.grantor_node_id IS NOT OLD.grantor_node_id
   OR NEW.grantor_authority_epoch IS NOT OLD.grantor_authority_epoch OR NEW.target_ref IS NOT OLD.target_ref
   OR NEW.launch_project_id IS NOT OLD.launch_project_id OR NEW.caller_session_id IS NOT OLD.caller_session_id
   OR NEW.idempotency_key IS NOT OLD.idempotency_key OR NEW.request_digest IS NOT OLD.request_digest
   OR NEW.session_id IS NOT OLD.session_id OR NEW.created_at IS NOT OLD.created_at
   OR (OLD.policy_request_json IS NOT NULL AND NEW.policy_request_json IS NOT OLD.policy_request_json)
 BEGIN SELECT RAISE(ABORT,'a portfolio appointment identity is immutable'); END;
CREATE TRIGGER manager_portfolio_appointments_appointed_final BEFORE UPDATE ON manager_portfolio_appointments
 WHEN OLD.state='appointed'
 BEGIN SELECT RAISE(ABORT,'an appointed portfolio appointment is final'); END;";
// RSI-RELEASED-MIGRATION-END: v156-portfolio-appointments

impl Store {
    fn migrate_v156(&self, version: i32) -> Result<()> {
        // The number is the rolling head + 1 at landing (S4's M2 is V155);
        // the guard accepts V154 or V155 below it, so this file applies with
        // or without V155 present.
        if version < 156 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if !(154..156).contains(&prior) {
                return Err(DaemonError::Store(format!(
                    "the portfolio appointment table requires V154 or V155, found V{prior}"
                )));
            }
            tx.execute_batch(V156_PORTFOLIO_APPOINTMENTS)?;
            tx.pragma_update(None, "user_version", 156)?;
            tx.commit()?;
        }
        Ok(())
    }
}
