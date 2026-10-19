// rsi-migration: additive
/// #2649: standing operator directives, durable state on a manager scope (a
/// project, an area or a portfolio node), not on a session, so a directive
/// outlives every seat rotation without a hand-copied handoff. A row is never
/// deleted: `cleared_at` and `expires_at` end its delivery and the row stays
/// as the audit of who said what, when and through whom.
/// `provenance` is required when `author` is a session (a parent manager
/// relaying an operator instruction); the operator's own rows may omit it.
/// `(author, idempotency_key)` makes a replayed write return the first row.
impl Store {
    fn migrate_v178(&self, version: i32) -> Result<()> {
        if version < 178 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE standing_directives (
                    id TEXT PRIMARY KEY NOT NULL CHECK(rsi_uuid_is_canonical(id)),
                    scope TEXT NOT NULL
                        CHECK(scope GLOB 'project:*' OR scope GLOB 'area:*' OR scope GLOB 'portfolio:*'),
                    body TEXT NOT NULL CHECK(length(trim(body)) > 0 AND length(body) <= 2048),
                    author TEXT NOT NULL CHECK(author = 'operator' OR author GLOB 'session:*'),
                    provenance TEXT
                        CHECK(provenance IS NULL OR length(provenance) BETWEEN 1 AND 512),
                    idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 128),
                    request_digest TEXT NOT NULL,
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    expires_at TEXT
                        CHECK(expires_at IS NULL OR rsi_rfc3339_nanos_is_canonical(expires_at)),
                    cleared_at TEXT
                        CHECK(cleared_at IS NULL OR rsi_rfc3339_nanos_is_canonical(cleared_at)),
                    cleared_by TEXT
                        CHECK(cleared_by IS NULL OR cleared_by = 'operator' OR cleared_by GLOB 'session:*'),
                    CHECK((cleared_at IS NULL) = (cleared_by IS NULL)),
                    CHECK(author = 'operator' OR provenance IS NOT NULL),
                    UNIQUE(author, idempotency_key)
                );
                CREATE INDEX standing_directives_scope_active
                    ON standing_directives(scope, created_at) WHERE cleared_at IS NULL;",
            )?;
            tx.pragma_update(None, "user_version", 178)?;
            tx.commit()?;
        }
        Ok(())
    }
}
