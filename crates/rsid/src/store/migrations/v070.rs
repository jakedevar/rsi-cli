impl Store {
    fn migrate_v070(&self, version: i32) -> Result<()> {
        // V70: accumulated active-work milliseconds per session (TD1 / TUI redesign).
        // "Active work" = wall-clock Running interval MINUS AskUserQuestion wait, folded
        // on the monitor snapshot tick and at finalize. Nullable for symmetry with V41
        // approval_wait_ms: NULL = unmeasured (pre-V70 row, or container that never
        // entered the monitor loop), Some(0) = measured-but-no-work-yet. Add-only floor —
        // never recomputed from wall-clock timestamps, so it stays monotonic across daemon
        // restart (restore flips Running→Failed WITHOUT finalize; the last-flushed floor
        // stands, the crash tail is accepted bounded loss).
        if version < 70 {
            self.add_column_if_not_exists("sessions", "work_time_ms", "INTEGER")?;
            tracing::info!("V70 migration complete: work_time_ms column");
            self.conn.pragma_update(None, "user_version", 70)?;
        }

        Ok(())
    }
}
