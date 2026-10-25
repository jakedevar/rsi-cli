// rsi-migration: additive
/// #3235: retain the latest retirement per appointment so predecessor reads
/// seek at most 33 index entries, regardless of scope edits or old appointments.
/// The one-time backfill aggregates history; subsequent inserts maintain the
/// projection transactionally, including out-of-order retirement timestamps.
impl Store {
    fn migrate_v184(&self, version: i32) -> Result<()> {
        if version < 184 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE harness_manager_retired_appointments (
                    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)),
                    manager_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(manager_session_id)),
                    retired_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(retired_at)),
                    PRIMARY KEY(project_id,manager_session_id)
                ) WITHOUT ROWID;
                INSERT INTO harness_manager_retired_appointments
                    SELECT project_id,manager_session_id,MAX(retired_at)
                    FROM harness_manager_notice_scope_retirements
                    GROUP BY project_id,manager_session_id;
                CREATE INDEX harness_manager_retired_appointments_recent
                    ON harness_manager_retired_appointments(
                        project_id,retired_at DESC,manager_session_id);
                CREATE TRIGGER harness_manager_retired_appointments_insert
                    AFTER INSERT ON harness_manager_notice_scope_retirements
                    BEGIN
                        INSERT INTO harness_manager_retired_appointments(
                            project_id,manager_session_id,retired_at)
                        VALUES(NEW.project_id,NEW.manager_session_id,NEW.retired_at)
                        ON CONFLICT(project_id,manager_session_id) DO UPDATE
                            SET retired_at=excluded.retired_at
                            WHERE excluded.retired_at > harness_manager_retired_appointments.retired_at;
                    END;
                CREATE TRIGGER harness_manager_retired_appointments_forward
                    BEFORE UPDATE ON harness_manager_retired_appointments
                    WHEN NEW.project_id IS NOT OLD.project_id
                        OR NEW.manager_session_id IS NOT OLD.manager_session_id
                        OR NEW.retired_at <= OLD.retired_at
                    BEGIN SELECT RAISE(ABORT,'retired appointments are forward only'); END;
                CREATE TRIGGER harness_manager_retired_appointments_no_delete
                    BEFORE DELETE ON harness_manager_retired_appointments
                    BEGIN SELECT RAISE(ABORT,'retired appointments are retained'); END;",
            )?;
            tx.pragma_update(None, "user_version", 184)?;
            tx.commit()?;
        }
        Ok(())
    }
}
