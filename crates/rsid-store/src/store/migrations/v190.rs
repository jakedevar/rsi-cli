// rsi-migration: additive
/// Hub #3345: one row per satellite ENQUEUE report the hub daemon takes in
/// itself (fetch, conflict probe, enqueue, reply), so the intake is exactly-once
/// across manager seat rotations and restarts and each reply is sent once.
impl Store {
    fn migrate_v190(&self, version: i32) -> Result<()> {
        if version < 190 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE satellite_enqueue_intakes (
                    peer_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(peer_id)),
                    report_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(report_id)),
                    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)),
                    source_commit TEXT NOT NULL CHECK(length(source_commit)=40),
                    branch TEXT,
                    issue INTEGER CHECK(issue IS NULL OR issue>0),
                    fixes_issue INTEGER CHECK(fixes_issue IS NULL OR fixes_issue>0),
                    fixes_tests_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(fixes_tests_json)),
                    state TEXT NOT NULL CHECK(state IN
                        ('received','enqueued','conflicts','refused','needs_review','settled')),
                    queue_entry_id TEXT CHECK(queue_entry_id IS NULL
                        OR rsi_uuid_is_canonical(queue_entry_id)),
                    outcome_code TEXT,
                    outcome_json TEXT CHECK(outcome_json IS NULL OR json_valid(outcome_json)),
                    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts>=0),
                    notified_state TEXT,
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
                    PRIMARY KEY(peer_id,report_id)
                ) WITHOUT ROWID;
                CREATE INDEX satellite_enqueue_intakes_open
                    ON satellite_enqueue_intakes(created_at,peer_id,report_id)
                    WHERE state IN ('received','enqueued')
                       OR notified_state IS NOT state;",
            )?;
            tx.pragma_update(None, "user_version", 190)?;
            tx.commit()?;
        }
        Ok(())
    }
}
