// rsi-migration: additive
/// #2453: the minimal durable obligation of a queued satellite report. The
/// outbox is process-local, so without this row a report still unacknowledged
/// at a restart vanished without a trace; with it the satellite tells its
/// manager the report may not have reached the hub. No report text is kept.
/// `held_reason`, `refusal_reason` and `undelivered_reason` are the typed failures
/// observed for the report and not yet recorded as a notice for a currently
/// authorized seat: each is cleared only once its notice is durable, so no
/// acknowledgement, restart or seat gap discharges an owed failure.
/// `attempted_at` rotates unroutable rows to the back so a blocked prefix
/// cannot starve the rest.
impl Store {
    fn migrate_v177(&self, version: i32) -> Result<()> {
        if version < 177 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE satellite_report_obligations (
                    report_id TEXT PRIMARY KEY NOT NULL
                        CHECK(rsi_uuid_is_canonical(report_id)),
                    project_id TEXT
                        CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
                    kind TEXT NOT NULL CHECK(kind IN ('enqueue','deploy_ready','result')),
                    queued_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(queued_at)),
                    settled_at TEXT
                        CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
                    held_reason TEXT
                        CHECK(held_reason IS NULL OR length(held_reason) BETWEEN 1 AND 64),
                    refusal_reason TEXT
                        CHECK(refusal_reason IS NULL OR length(refusal_reason) BETWEEN 1 AND 64),
                    undelivered_reason TEXT
                        CHECK(undelivered_reason IS NULL OR length(undelivered_reason) BETWEEN 1 AND 64),
                    attempted_at TEXT
                        CHECK(attempted_at IS NULL OR rsi_rfc3339_nanos_is_canonical(attempted_at))
                );
                CREATE INDEX satellite_report_obligations_open
                    ON satellite_report_obligations(queued_at) WHERE settled_at IS NULL;",
            )?;
            tx.pragma_update(None, "user_version", 177)?;
            tx.commit()?;
        }
        Ok(())
    }
}
