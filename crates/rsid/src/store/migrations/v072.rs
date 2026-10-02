impl Store {
    fn migrate_v072(&self, version: i32) -> Result<()> {
        // V72: local issue tracker foundation (C1) — `issues` + `issue_deps`.
        //
        // V71 HOLE: V71 is HELD for the unrelated S11 program. Any DB that
        // reaches 72 here will NEVER execute a later-inserted `if version < 71`
        // block — S11 must write idempotent DDL that also applies correctly
        // when first run at user_version > 71, or take a fresh number.
        //
        // Net-new tables, disjoint from V33 `issue_tracker_dispatches` and the
        // session issue columns. `status` carries NO SQL CHECK on purpose: a
        // CHECK cannot be altered without a table rebuild and C2/C5 may add
        // variants; validation lives in Rust (serde-exact strings, unknown
        // string => store error). "Blocked" is derived from `issue_deps`
        // edges, never stored. FK enforcement relies on the
        // `PRAGMA foreign_keys=ON` set in both `open` and `open_in_memory`.
        // All DDL is IF NOT EXISTS-guarded and the version bump is last, so a
        // crash mid-block re-runs cleanly on next boot.
        if version < 72 {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS issues (
                    id TEXT PRIMARY KEY,                     -- lowercase canonical UUID
                    display_number INTEGER NOT NULL UNIQUE,  -- monotonic human number
                    title TEXT NOT NULL,
                    body TEXT NOT NULL DEFAULT '',
                    status TEXT NOT NULL DEFAULT 'Open',     -- serde variants: Open|InProgress|Closed|Cancelled
                    priority INTEGER,                        -- 1=urgent..4=low; NULL=unset
                    labels TEXT NOT NULL DEFAULT '[]',       -- JSON array of strings
                    created_by_session_id TEXT,              -- lowercase UUID; NULL=operator
                    assignee TEXT,
                    created_at TEXT NOT NULL,                -- RFC3339 nanos
                    updated_at TEXT NOT NULL,
                    closed_at TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
                CREATE INDEX IF NOT EXISTS idx_issues_created_by ON issues(created_by_session_id);

                CREATE TABLE IF NOT EXISTS issue_deps (
                    issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
                    depends_on_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (issue_id, depends_on_id),
                    CHECK (issue_id <> depends_on_id)
                );
                CREATE INDEX IF NOT EXISTS idx_issue_deps_depends_on ON issue_deps(depends_on_id);
                ",
            )?;
            tracing::info!("V72 migration complete: issues + issue_deps tables");
            self.conn.pragma_update(None, "user_version", 72)?;
        }

        Ok(())
    }
}
