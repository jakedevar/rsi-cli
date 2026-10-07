/// Issue #1333 (friction telemetry, the "andon"): two append-only tables.
/// `friction_events` holds one row per recorded friction signal: a signature
/// of code tokens (`agent_refusal:AgentGetIssue:agent_issue_authority_denied`),
/// the session and project it belongs to, and one evidence reference
/// (`deploy:<uuid>`). The CHECKs admit code tokens and ids only, never prose.
/// `andon_filings` holds the one kaizen Issue the andon filed per
/// `(project, signature)`; its primary key is the dedupe. Neither table is
/// ever updated, deleted or replaced: `INSERT OR REPLACE` (and `REPLACE`)
/// deletes the conflicting row without firing DELETE triggers while
/// `recursive_triggers` is off, so a BEFORE INSERT guard refuses any insert
/// that collides with a retained row's key (#1396, the V152 pattern).
// RSI-RELEASED-MIGRATION-BEGIN: v157-friction-andon
const V157_FRICTION_ANDON: &str = "CREATE TABLE friction_events (
    id INTEGER PRIMARY KEY,
    signature TEXT NOT NULL CHECK(length(signature) BETWEEN 3 AND 160
        AND instr(signature, ':') > 1 AND signature NOT GLOB '*[^A-Za-z0-9_:]*'),
    session_id TEXT CHECK(session_id IS NULL OR rsi_uuid_is_canonical(session_id)),
    project_id TEXT CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
    evidence_ref TEXT CHECK(evidence_ref IS NULL OR (length(evidence_ref) BETWEEN 3 AND 128
        AND instr(evidence_ref, ':') > 1 AND evidence_ref NOT GLOB '*[^A-Za-z0-9_:.-]*')),
    recorded_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(recorded_at))
);
CREATE INDEX friction_events_by_signature ON friction_events(project_id, signature, recorded_at);
CREATE INDEX friction_events_by_time ON friction_events(recorded_at);
CREATE TRIGGER friction_events_no_update BEFORE UPDATE ON friction_events
 BEGIN SELECT RAISE(ABORT,'friction events are append-only'); END;
CREATE TRIGGER friction_events_no_delete BEFORE DELETE ON friction_events
 BEGIN SELECT RAISE(ABORT,'friction events are retained'); END;
CREATE TRIGGER friction_events_no_replace BEFORE INSERT ON friction_events
 WHEN EXISTS(SELECT 1 FROM friction_events WHERE id = NEW.id)
 BEGIN SELECT RAISE(ABORT,'friction events are never replaced'); END;
CREATE TABLE andon_filings (
    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)),
    signature TEXT NOT NULL CHECK(length(signature) BETWEEN 3 AND 160
        AND instr(signature, ':') > 1 AND signature NOT GLOB '*[^A-Za-z0-9_:]*'),
    issue_id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(issue_id)),
    occurrences INTEGER NOT NULL CHECK(occurrences > 0),
    sessions INTEGER NOT NULL CHECK(sessions > 0),
    filed_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(filed_at)),
    PRIMARY KEY(project_id, signature)
);
CREATE INDEX andon_filings_by_time ON andon_filings(filed_at);
CREATE TRIGGER andon_filings_no_update BEFORE UPDATE ON andon_filings
 BEGIN SELECT RAISE(ABORT,'andon filings are append-only'); END;
CREATE TRIGGER andon_filings_no_delete BEFORE DELETE ON andon_filings
 BEGIN SELECT RAISE(ABORT,'andon filings are retained'); END;
CREATE TRIGGER andon_filings_no_replace BEFORE INSERT ON andon_filings
 WHEN EXISTS(SELECT 1 FROM andon_filings
              WHERE (project_id = NEW.project_id AND signature = NEW.signature)
                 OR issue_id = NEW.issue_id)
 BEGIN SELECT RAISE(ABORT,'andon filings are never replaced'); END;";
// RSI-RELEASED-MIGRATION-END: v157-friction-andon

impl Store {
    fn migrate_v157(&self, version: i32) -> Result<()> {
        if version < 157 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if prior != 156 {
                return Err(DaemonError::Store(format!(
                    "the friction telemetry tables require V156, found V{prior}"
                )));
            }
            tx.execute_batch(V157_FRICTION_ANDON)?;
            tx.pragma_update(None, "user_version", 157)?;
            tx.commit()?;
        }
        Ok(())
    }
}
