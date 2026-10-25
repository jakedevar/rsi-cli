// rsi-migration: additive
/// #426: session-owned multipart preamble debt, including compact/clear fences.
impl Store {
    fn migrate_v189(&self, version: i32) -> Result<()> {
        if version < 189 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE session_start_preambles (
                    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    state_json TEXT CHECK(state_json IS NULL OR json_valid(state_json)),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
                ) WITHOUT ROWID;",
            )?;
            tx.pragma_update(None, "user_version", 189)?;
            tx.commit()?;
        }
        Ok(())
    }
}
