impl Store {
    fn migrate_v103(&self, version: i32) -> Result<()> {
        // V103: successful manager-lineage rotation receipts and notice CAS.
        if version < 103 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE harness_manager_rotation_edges (
                    predecessor_session_id TEXT NOT NULL REFERENCES sessions(id),
                    successor_session_id TEXT NOT NULL REFERENCES sessions(id),
                    committed_at TEXT NOT NULL,
                    retired_at TEXT,
                    PRIMARY KEY(predecessor_session_id,successor_session_id),
                    CHECK(predecessor_session_id<>successor_session_id)
                 );
                 CREATE UNIQUE INDEX harness_manager_rotation_current
                    ON harness_manager_rotation_edges(predecessor_session_id) WHERE retired_at IS NULL;
                 CREATE TRIGGER harness_manager_rotation_retire_on_restore
                    AFTER UPDATE OF status ON sessions
                    WHEN OLD.status='Archived' AND NEW.status<>'Archived'
                    BEGIN UPDATE harness_manager_rotation_edges SET retired_at=NEW.updated_at
                      WHERE predecessor_session_id=NEW.id AND retired_at IS NULL; END;
                 ALTER TABLE harness_manager_watches ADD COLUMN notice_generation INTEGER NOT NULL DEFAULT 1
                    CHECK(typeof(notice_generation)='integer' AND notice_generation>0);
                 CREATE TRIGGER harness_manager_watch_generation_after_update
                    AFTER UPDATE ON scheduled_jobs
                    BEGIN UPDATE harness_manager_watches SET notice_generation=notice_generation+1
                      WHERE job_id=NEW.id; END;",
            )?;
            tx.execute("PRAGMA user_version = 103", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
