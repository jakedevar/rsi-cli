/// The operator to-do list (#1747): the things only the operator can do,
/// written by managers (and by the operator) and shown wherever the operator is.
impl Store {
    fn migrate_v163(&self, version: i32) -> Result<()> {
        if version < 163 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE operator_todos (
                    id TEXT PRIMARY KEY NOT NULL CHECK(rsi_uuid_is_canonical(id)),
                    scope TEXT NOT NULL CHECK(length(scope) BETWEEN 1 AND 80 AND instr(scope,char(0))=0),
                    project_id TEXT CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
                    todo_key TEXT NOT NULL CHECK(length(CAST(todo_key AS BLOB)) BETWEEN 1 AND 128 AND instr(todo_key,char(0))=0),
                    title TEXT NOT NULL CHECK(length(CAST(title AS BLOB)) BETWEEN 1 AND 512 AND instr(title,char(0))=0),
                    how TEXT NOT NULL CHECK(length(CAST(how AS BLOB)) BETWEEN 1 AND 2048 AND instr(how,char(0))=0),
                    link TEXT CHECK(link IS NULL OR (length(CAST(link AS BLOB)) BETWEEN 1 AND 512 AND instr(link,char(0))=0)),
                    priority INTEGER NOT NULL DEFAULT 3 CHECK(priority BETWEEN 1 AND 4),
                    added_by TEXT NOT NULL CHECK(length(added_by) BETWEEN 1 AND 80),
                    status TEXT NOT NULL CHECK(status IN ('open','done','dismissed')),
                    evidence TEXT CHECK(evidence IS NULL OR (length(CAST(evidence AS BLOB)) BETWEEN 1 AND 2048 AND instr(evidence,char(0))=0)),
                    evidence_observed INTEGER NOT NULL DEFAULT 0 CHECK(evidence_observed IN (0,1)),
                    closed_by TEXT CHECK(closed_by IS NULL OR length(closed_by) BETWEEN 1 AND 80),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
                    closed_at TEXT CHECK(closed_at IS NULL OR rsi_rfc3339_nanos_is_canonical(closed_at)),
                    UNIQUE(scope,todo_key),
                    CHECK((status='open' AND closed_at IS NULL AND closed_by IS NULL)
                        OR (status<>'open' AND closed_at IS NOT NULL AND closed_by IS NOT NULL)),
                    -- An agent never closes an item without recorded evidence, and
                    -- never calls an item done unless it observed that it was done.
                    CHECK(status='open' OR closed_by NOT LIKE 'session:%' OR evidence IS NOT NULL),
                    CHECK(status<>'done' OR closed_by NOT LIKE 'session:%' OR evidence_observed=1)
                );
                CREATE INDEX operator_todos_open
                    ON operator_todos(priority,created_at DESC) WHERE status='open';
                CREATE INDEX operator_todos_closed
                    ON operator_todos(closed_at) WHERE status<>'open';
                CREATE TRIGGER operator_todos_immutable
                    BEFORE UPDATE ON operator_todos
                    WHEN NEW.id IS NOT OLD.id
                        OR NEW.scope IS NOT OLD.scope
                        OR NEW.todo_key IS NOT OLD.todo_key
                        OR NEW.added_by IS NOT OLD.added_by
                        OR NEW.created_at IS NOT OLD.created_at
                    BEGIN SELECT RAISE(ABORT,'operator todo identity is immutable'); END;
                CREATE TRIGGER operator_todos_closed_terminal
                    BEFORE UPDATE ON operator_todos
                    WHEN OLD.status<>'open'
                    BEGIN SELECT RAISE(ABORT,'operator todo is closed'); END;
                CREATE TABLE operator_todo_receipts (
                    principal TEXT NOT NULL CHECK(length(principal) BETWEEN 1 AND 80),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 128 AND instr(idempotency_key,char(0))=0),
                    request_fingerprint TEXT NOT NULL,
                    todo_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(todo_id)) REFERENCES operator_todos(id) ON DELETE RESTRICT,
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    PRIMARY KEY(principal,idempotency_key)
                );",
            )?;
            tx.pragma_update(None, "user_version", 163)?;
            tx.commit()?;
        }
        Ok(())
    }
}
