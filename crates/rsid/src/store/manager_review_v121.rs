//! V121 exact-source manager review assignments and immutable receipts.

use super::{Store, source_worktree_v120};
use crate::error::{DaemonError, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

// RSI-RELEASED-MIGRATION-BEGIN: v121-manager-review-catalog
pub(crate) const V121_CATALOG_OBJECTS: [(&str, &str); 19] = [
    ("table", "manager_review_assignments"),
    ("table", "manager_review_receipts"),
    ("table", "manager_review_findings"),
    ("index", "manager_review_assignments_one_current"),
    ("index", "manager_review_assignments_action"),
    ("index", "manager_review_assignments_reconcile"),
    ("index", "manager_review_receipts_source_invocation"),
    ("index", "manager_review_receipts_idempotency"),
    ("index", "manager_review_findings_blocking"),
    ("trigger", "manager_review_assignments_v121_no_delete"),
    (
        "trigger",
        "manager_review_assignments_v121_identity_immutable",
    ),
    ("trigger", "manager_review_assignments_v121_forward"),
    ("trigger", "manager_review_receipts_v121_insert_guard"),
    ("trigger", "manager_review_receipts_v121_no_update"),
    ("trigger", "manager_review_receipts_v121_no_delete"),
    ("trigger", "manager_review_findings_v121_insert_guard"),
    ("trigger", "manager_review_findings_v121_no_update"),
    ("trigger", "manager_review_findings_v121_no_delete"),
    ("trigger", "manager_review_assignments_v121_submitted_guard"),
];

pub(crate) const V121_CATALOG_FINGERPRINT: &str =
    "sha256:5977da1d6fe8451d5b75767849fef31d6022d618b6fe52d145dcdd7f1990c7d1";
// RSI-RELEASED-MIGRATION-END: v121-manager-review-catalog

fn catalog_fingerprint(connection: &Connection) -> Result<String> {
    let mut hash = Sha256::new();
    for (kind, name) in V121_CATALOG_OBJECTS {
        let sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                [kind, name],
                |row| row.get(0),
            )
            .map_err(|_| {
                DaemonError::Store(format!("V121 catalog object missing: {kind} {name}"))
            })?;
        hash.update(kind.as_bytes());
        hash.update([0]);
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(sql.as_bytes());
        hash.update([0xff]);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

pub(crate) fn validate_v121_catalog(connection: &Connection) -> Result<()> {
    let actual = catalog_fingerprint(connection)?;
    if actual != V121_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V121 manager review catalog fingerprint mismatch: {actual}"
        )));
    }
    Ok(())
}

// RSI-RELEASED-MIGRATION-BEGIN: v121-manager-review-migration
pub(crate) fn apply_v121_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 120 {
        return Err(DaemonError::Store(format!(
            "V121 requires exact V120 source, found V{version}"
        )));
    }
    source_worktree_v120::validate_v120_catalog(&tx)?;
    tx.execute_batch(
        "CREATE TABLE manager_review_assignments (
            assignment_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(assignment_id)),
            project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
            epic_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            manager_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            scope_version INTEGER NOT NULL CHECK(scope_version>0),
            work_key TEXT NOT NULL CHECK(length(work_key) BETWEEN 1 AND 256),
            spec_revision INTEGER NOT NULL CHECK(spec_revision>0),
            author_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            source_sha TEXT NOT NULL CHECK(length(source_sha)=40 AND source_sha=lower(source_sha) AND source_sha NOT GLOB '*[^0-9a-f]*'),
            reviewer_session_id TEXT CHECK(reviewer_session_id IS NULL OR rsi_uuid_is_canonical(reviewer_session_id)),
            reviewer_invocation_id TEXT REFERENCES model_invocations(id) ON DELETE RESTRICT,
            reviewer_custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
            reviewer_custody_generation INTEGER CHECK(reviewer_custody_generation IS NULL OR reviewer_custody_generation>0),
            action_operation_id TEXT REFERENCES harness_manager_v2_operations(id) ON DELETE RESTRICT,
            state TEXT NOT NULL CHECK(state IN ('reserved','allocating','active','submitted','superseded','cancelled','failed')),
            row_version INTEGER NOT NULL CHECK(row_version>0),
            request_json TEXT NOT NULL CHECK(json_valid(request_json)),
            request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8)=lower(substr(request_fingerprint,8)) AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
            superseded_by_assignment_id TEXT REFERENCES manager_review_assignments(assignment_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            failure_code TEXT CHECK(failure_code IS NULL OR length(failure_code) BETWEEN 1 AND 128),
            created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            terminal_at TEXT CHECK(terminal_at IS NULL OR rsi_rfc3339_nanos_is_canonical(terminal_at)),
            CHECK(author_session_id IS NOT reviewer_session_id),
            CHECK((state='reserved' AND reviewer_session_id IS NULL AND reviewer_invocation_id IS NULL AND reviewer_custody_id IS NULL AND reviewer_custody_generation IS NULL AND action_operation_id IS NULL)
               OR (state='allocating' AND reviewer_session_id IS NOT NULL AND action_operation_id IS NOT NULL AND reviewer_invocation_id IS NULL AND reviewer_custody_id IS NULL AND reviewer_custody_generation IS NULL)
               OR (state IN ('active','submitted') AND reviewer_session_id IS NOT NULL AND action_operation_id IS NOT NULL AND reviewer_invocation_id IS NOT NULL AND reviewer_custody_id IS NOT NULL AND reviewer_custody_generation IS NOT NULL)
               OR state IN ('superseded','cancelled','failed')),
            CHECK((state IN ('submitted','superseded','cancelled','failed') AND terminal_at IS NOT NULL)
               OR (state IN ('reserved','allocating','active') AND terminal_at IS NULL)),
            CHECK((state='superseded' AND superseded_by_assignment_id IS NOT NULL)
               OR (state!='superseded' AND superseded_by_assignment_id IS NULL)),
            CHECK((state='failed' AND failure_code IS NOT NULL) OR (state!='failed' AND failure_code IS NULL))
        );
        CREATE TABLE manager_review_receipts (
            receipt_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(receipt_id)),
            assignment_id TEXT NOT NULL UNIQUE REFERENCES manager_review_assignments(assignment_id) ON DELETE RESTRICT,
            source_sha TEXT NOT NULL CHECK(length(source_sha)=40 AND source_sha=lower(source_sha) AND source_sha NOT GLOB '*[^0-9a-f]*'),
            reviewer_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            reviewer_invocation_id TEXT NOT NULL REFERENCES model_invocations(id) ON DELETE RESTRICT,
            reviewer_custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
            reviewer_custody_generation INTEGER NOT NULL CHECK(reviewer_custody_generation>0),
            verdict TEXT NOT NULL CHECK(verdict IN ('accepted','changes_requested','blocked')),
            idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 128),
            request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8)=lower(substr(request_fingerprint,8)) AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
            created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at))
        );
        CREATE TABLE manager_review_findings (
            receipt_id TEXT NOT NULL REFERENCES manager_review_receipts(receipt_id) ON DELETE RESTRICT,
            finding_key TEXT NOT NULL CHECK(length(finding_key) BETWEEN 1 AND 64 AND finding_key NOT GLOB '*[^A-Za-z0-9_-]*'),
            severity TEXT NOT NULL CHECK(severity IN ('info','warning','error')),
            summary TEXT NOT NULL CHECK(length(summary) BETWEEN 1 AND 2048 AND instr(summary,char(0))=0),
            location TEXT CHECK(location IS NULL OR (length(location) BETWEEN 1 AND 1024 AND instr(location,char(0))=0)),
            blocking INTEGER NOT NULL CHECK(blocking IN (0,1)),
            PRIMARY KEY(receipt_id,finding_key)
        );
        CREATE UNIQUE INDEX manager_review_assignments_one_current
            ON manager_review_assignments(project_id,epic_id,work_key,spec_revision,source_sha)
            WHERE superseded_by_assignment_id IS NULL;
        CREATE UNIQUE INDEX manager_review_assignments_action
            ON manager_review_assignments(action_operation_id)
            WHERE action_operation_id IS NOT NULL;
        CREATE INDEX manager_review_assignments_reconcile
            ON manager_review_assignments(state,created_at,assignment_id);
        CREATE UNIQUE INDEX manager_review_receipts_source_invocation
            ON manager_review_receipts(source_sha,reviewer_invocation_id);
        CREATE UNIQUE INDEX manager_review_receipts_idempotency
            ON manager_review_receipts(reviewer_session_id,idempotency_key);
        CREATE INDEX manager_review_findings_blocking
            ON manager_review_findings(receipt_id,blocking,finding_key);
        CREATE TRIGGER manager_review_assignments_v121_no_delete
        BEFORE DELETE ON manager_review_assignments BEGIN
            SELECT RAISE(ABORT,'manager review assignments are retained');
        END;
        CREATE TRIGGER manager_review_assignments_v121_identity_immutable
        BEFORE UPDATE ON manager_review_assignments
        WHEN NEW.assignment_id!=OLD.assignment_id OR NEW.project_id!=OLD.project_id
          OR NEW.epic_id!=OLD.epic_id OR NEW.manager_session_id!=OLD.manager_session_id
          OR NEW.scope_version!=OLD.scope_version OR NEW.work_key!=OLD.work_key
          OR NEW.spec_revision!=OLD.spec_revision OR NEW.author_session_id!=OLD.author_session_id
          OR NEW.source_sha!=OLD.source_sha OR NEW.request_json!=OLD.request_json
          OR NEW.request_fingerprint!=OLD.request_fingerprint OR NEW.created_at!=OLD.created_at
        BEGIN SELECT RAISE(ABORT,'manager review assignment identity is immutable'); END;
        CREATE TRIGGER manager_review_assignments_v121_forward
        BEFORE UPDATE ON manager_review_assignments
        WHEN NEW.row_version!=OLD.row_version+1 OR NEW.updated_at=OLD.updated_at
          OR NOT ((OLD.state='reserved' AND NEW.state IN ('allocating','superseded','cancelled','failed'))
              OR (OLD.state='allocating' AND NEW.state IN ('active','superseded','cancelled','failed'))
              OR (OLD.state='active' AND NEW.state IN ('submitted','superseded','cancelled','failed'))
              OR (OLD.state IN ('submitted','cancelled','failed') AND NEW.state='superseded'))
        BEGIN SELECT RAISE(ABORT,'manager review assignment transition is not forward'); END;
        CREATE TRIGGER manager_review_receipts_v121_insert_guard
        BEFORE INSERT ON manager_review_receipts
        WHEN NOT EXISTS(
            SELECT 1 FROM manager_review_assignments a
             WHERE a.assignment_id=NEW.assignment_id AND a.state='active'
               AND a.source_sha=NEW.source_sha
               AND a.reviewer_session_id=NEW.reviewer_session_id
               AND a.reviewer_invocation_id=NEW.reviewer_invocation_id
               AND a.reviewer_custody_id=NEW.reviewer_custody_id
               AND a.reviewer_custody_generation=NEW.reviewer_custody_generation)
        BEGIN SELECT RAISE(ABORT,'manager review receipt assignment mismatch'); END;
        CREATE TRIGGER manager_review_receipts_v121_no_update
        BEFORE UPDATE ON manager_review_receipts BEGIN
            SELECT RAISE(ABORT,'manager review receipts are immutable');
        END;
        CREATE TRIGGER manager_review_receipts_v121_no_delete
        BEFORE DELETE ON manager_review_receipts BEGIN
            SELECT RAISE(ABORT,'manager review receipts are retained');
        END;
        CREATE TRIGGER manager_review_findings_v121_insert_guard
        BEFORE INSERT ON manager_review_findings
        WHEN (SELECT count(*) FROM manager_review_findings WHERE receipt_id=NEW.receipt_id)>=64
        BEGIN SELECT RAISE(ABORT,'manager review finding limit exceeded'); END;
        CREATE TRIGGER manager_review_findings_v121_no_update
        BEFORE UPDATE ON manager_review_findings BEGIN
            SELECT RAISE(ABORT,'manager review findings are immutable');
        END;
        CREATE TRIGGER manager_review_findings_v121_no_delete
        BEFORE DELETE ON manager_review_findings BEGIN
            SELECT RAISE(ABORT,'manager review findings are retained');
        END;
        CREATE TRIGGER manager_review_assignments_v121_submitted_guard
        BEFORE UPDATE OF state ON manager_review_assignments
        WHEN NEW.state='submitted' AND NOT EXISTS(
            SELECT 1 FROM manager_review_receipts r WHERE r.assignment_id=OLD.assignment_id)
        BEGIN SELECT RAISE(ABORT,'manager review submitted state requires receipt'); END;",
    )?;
    let foreign_key_errors: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0 {
        return Err(DaemonError::Store(format!(
            "V121 manager review migration found {foreign_key_errors} foreign-key violation(s)"
        )));
    }
    tx.execute("PRAGMA user_version = 121", [])?;
    let actual = catalog_fingerprint(&tx)?;
    if actual != V121_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V121 manager review catalog fingerprint mismatch: {actual}"
        )));
    }
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v121-manager-review-migration

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v121_catalog_is_present_and_authenticated() {
        let store = Store::open_in_memory().expect("open current store");
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            super::super::LATEST_SCHEMA_VERSION
        );
        validate_v121_catalog(&store.conn).expect("validate V121 review catalog");
    }
}
