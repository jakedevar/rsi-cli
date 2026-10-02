impl Store {
    fn migrate_v099(&self, version: i32) -> Result<()> {
        // V99: consume what the provider CLI already sends. The Claude CLI
        // emits a rich `system/init` handshake, a `rate_limit_event` stream,
        // and a ~12-counter `result.usage` object; RSI read two init fields,
        // dropped rate limits on the floor, and persisted four usage counters.
        //
        // Additive only — nullable session/turn columns plus one new table. No
        // released DDL is touched and no spawn behaviour changes; this is a
        // receive-side migration. Existing rows stay NULL: none of these facts
        // is recoverable for a session that already ran.
        if version < 99 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

            // P1-A: what the CLI told us it is, and what it told us it supports.
            add_column_if_not_exists_tx(&tx, "sessions", "provider_cli_version", "TEXT")?;
            add_column_if_not_exists_tx(&tx, "sessions", "provider_capabilities", "TEXT")?;

            // P1-C: richer `result.usage` capture, session-level.
            add_column_if_not_exists_tx(&tx, "sessions", "thinking_tokens", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "service_tier", "TEXT")?;
            add_column_if_not_exists_tx(&tx, "sessions", "cache_creation_1h_tokens", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "cache_creation_5m_tokens", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "permission_denial_count", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "subagent_stats_json", "TEXT")?;
            add_column_if_not_exists_tx(&tx, "sessions", "queued_turn_count", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "terminal_reason", "TEXT")?;

            // P1-C: the same signals attributed per turn.
            add_column_if_not_exists_tx(
                &tx,
                "turn_metrics",
                "thinking_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            add_column_if_not_exists_tx(
                &tx,
                "turn_metrics",
                "cache_creation_1h_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            add_column_if_not_exists_tx(
                &tx,
                "turn_metrics",
                "cache_creation_5m_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            add_column_if_not_exists_tx(&tx, "turn_metrics", "service_tier", "TEXT")?;

            // P1-B: plan-window utilization is an ACCOUNT-level fact — every
            // concurrent session reports the same windows — so it is a
            // daemon-wide latest-wins snapshot keyed by provider and window,
            // not a set of session columns.
            //
            // `provider` is the serde string of `SessionProvider`, so a second
            // provider reporting windows needs no schema change.
            // `resets_at_epoch` holds the provider's raw unix seconds verbatim;
            // `observed_at` is the RFC3339-with-nanoseconds timestamp the
            // repo's timestamp rule requires.
            tx.execute(
                "CREATE TABLE IF NOT EXISTS provider_rate_limit_windows (
                     provider            TEXT    NOT NULL,
                     window_key          TEXT    NOT NULL,
                     utilization         REAL    NOT NULL,
                     resets_at_epoch     INTEGER,
                     status              TEXT,
                     rate_limit_type     TEXT,
                     overage_status      TEXT,
                     is_using_overage    INTEGER NOT NULL DEFAULT 0,
                     observed_at         TEXT    NOT NULL,
                     observed_session_id TEXT    REFERENCES sessions(id),
                     PRIMARY KEY (provider, window_key)
                 )",
                [],
            )?;

            tx.execute("PRAGMA user_version = 99", [])?;
            tx.commit()?;
            tracing::info!(
                "V99 migration complete: provider handshake, rate-limit windows, richer usage"
            );
        }

        Ok(())
    }
}
