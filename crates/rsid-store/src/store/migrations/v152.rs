impl Store {
    fn migrate_v152(&self, version: i32) -> Result<()> {
        // Issue #12: immutable wake-origin provenance. A session launched by a
        // `fresh`/`agent_fresh` wake is a parentless root; the arming session
        // owns it. The origin is recorded on the session's own insert (trigger)
        // so scheduled-job retention or disabling cannot revoke the ownership.
        // The table is insert-only: UPDATE, DELETE and replacement (including
        // `INSERT OR REPLACE`) are refused. Formats use the store's canonical
        // predicates, as every other table does. The version is provisional;
        // the lander assigns the final number.
        if version < 152 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "CREATE TABLE session_wake_origins (
                     session_id TEXT NOT NULL PRIMARY KEY
                         CHECK(rsi_uuid_is_canonical(session_id)),
                     origin_session_id TEXT NOT NULL
                         CHECK(rsi_uuid_is_canonical(origin_session_id)),
                     scheduled_job_id TEXT NOT NULL
                         CHECK(rsi_uuid_is_canonical(scheduled_job_id)),
                     recorded_at TEXT NOT NULL
                         CHECK(rsi_rfc3339_nanos_is_canonical(recorded_at))
                 );
                 CREATE INDEX session_wake_origins_by_origin
                     ON session_wake_origins(origin_session_id);
                 CREATE TRIGGER session_wake_origins_no_update
                     BEFORE UPDATE ON session_wake_origins
                     BEGIN SELECT RAISE(ABORT, 'session_wake_origins rows are immutable'); END;
                 CREATE TRIGGER session_wake_origins_no_delete
                     BEFORE DELETE ON session_wake_origins
                     BEGIN SELECT RAISE(ABORT, 'session_wake_origins rows are retained'); END;
                 CREATE TRIGGER session_wake_origins_no_replace
                     BEFORE INSERT ON session_wake_origins
                     WHEN EXISTS(SELECT 1 FROM session_wake_origins
                                  WHERE session_id = NEW.session_id)
                     BEGIN SELECT RAISE(ABORT, 'session_wake_origins rows are never replaced'); END;
                 CREATE TRIGGER session_wake_origins_record
                     AFTER INSERT ON sessions
                     WHEN NEW.scheduled_job_id IS NOT NULL
                          AND NOT EXISTS(SELECT 1 FROM session_wake_origins
                                          WHERE session_id = NEW.id)
                     BEGIN
                         INSERT INTO session_wake_origins
                             (session_id, origin_session_id, scheduled_job_id, recorded_at)
                         SELECT NEW.id, job.wake_session_id, job.id,
                                strftime('%Y-%m-%dT%H:%M:%f000000Z', 'now')
                           FROM scheduled_jobs AS job
                          WHERE job.id = NEW.scheduled_job_id
                            AND job.wake_mode IN ('fresh', 'agent_fresh')
                            AND rsi_uuid_is_canonical(NEW.id)
                            AND rsi_uuid_is_canonical(job.wake_session_id)
                            AND rsi_uuid_is_canonical(job.id);
                     END;
                 INSERT INTO session_wake_origins
                     (session_id, origin_session_id, scheduled_job_id, recorded_at)
                 SELECT s.id, job.wake_session_id, job.id,
                        strftime('%Y-%m-%dT%H:%M:%f000000Z', 'now')
                   FROM sessions AS s
                   JOIN scheduled_jobs AS job ON job.id = s.scheduled_job_id
                  WHERE job.wake_mode IN ('fresh', 'agent_fresh')
                    AND rsi_uuid_is_canonical(s.id)
                    AND rsi_uuid_is_canonical(job.wake_session_id)
                    AND rsi_uuid_is_canonical(job.id);",
            )?;
            tx.pragma_update(None, "user_version", 152)?;
            tx.commit()?;
        }

        Ok(())
    }
}
