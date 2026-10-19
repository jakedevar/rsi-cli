//! One bounded superseding attempt for pre-removal quarantine proof fences (#2098).
use super::*;

// RSI-RELEASED-MIGRATION-BEGIN: v171-settlement-reproof-schema
const OLD_TRIGGER_0: &str = r#"CREATE TRIGGER source_worktree_settlement_items_forward_phase BEFORE UPDATE ON source_worktree_settlement_items
            WHEN NEW.phase!=OLD.phase AND NOT (
                (OLD.phase='planned' AND NEW.phase IN ('intent_committed','refused','recovery_required','unattempted')) OR
                (OLD.phase='intent_committed' AND NEW.phase IN ('worktree_removed','refused','recovery_required','unattempted')) OR
                (OLD.phase='worktree_removed' AND NEW.phase IN ('branch_removed','recovery_required')) OR
                (OLD.phase='branch_removed' AND NEW.phase IN ('settled','recovery_required'))
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_phase_regression'); END"#;
const NEW_TRIGGER_0: &str = r#"CREATE TRIGGER source_worktree_settlement_items_forward_phase BEFORE UPDATE ON source_worktree_settlement_items
            WHEN NEW.phase!=OLD.phase AND NOT (
                (OLD.phase='planned' AND NEW.phase IN ('intent_committed','refused','recovery_required','unattempted')) OR
                (OLD.phase='intent_committed' AND NEW.phase IN ('worktree_removed','refused','recovery_required','unattempted')) OR
                (OLD.phase='worktree_removed' AND NEW.phase IN ('branch_removed','recovery_required')) OR
                (OLD.phase='branch_removed' AND NEW.phase IN ('settled','recovery_required'))
            )
            AND NOT coalesce((OLD.phase='recovery_required' AND OLD.refusal_code='recovery_proof_failed'
                AND OLD.before_observation IS NULL
                AND OLD.after_observation='quarantined worktree failed its removal-authority proof'
                AND NEW.phase='intent_committed' AND NEW.before_observation IS NULL
                AND NEW.refusal_code IS NULL AND NEW.after_observation='quarantine_reproof_authorized_v1'
                AND EXISTS (SELECT 1 FROM source_worktree_settlement_reproof_items a
                    WHERE a.run_id=OLD.run_id AND a.session_id=OLD.session_id
                      AND a.fenced_at=OLD.updated_at AND a.reopened_at=NEW.updated_at)),0)
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_phase_regression'); END"#;
const OLD_TRIGGER_1: &str = r#"CREATE TRIGGER source_worktree_settlement_items_terminal BEFORE UPDATE ON source_worktree_settlement_items
            WHEN OLD.phase IN ('settled','refused','recovery_required','unattempted') AND (
                NEW.phase!=OLD.phase OR NEW.before_observation IS NOT OLD.before_observation
                OR NEW.after_observation IS NOT OLD.after_observation
                OR NEW.refusal_code IS NOT OLD.refusal_code OR NEW.updated_at!=OLD.updated_at
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_terminal'); END"#;
const NEW_TRIGGER_1: &str = r#"CREATE TRIGGER source_worktree_settlement_items_terminal BEFORE UPDATE ON source_worktree_settlement_items
            WHEN OLD.phase IN ('settled','refused','recovery_required','unattempted') AND (
                NEW.phase!=OLD.phase OR NEW.before_observation IS NOT OLD.before_observation
                OR NEW.after_observation IS NOT OLD.after_observation
                OR NEW.refusal_code IS NOT OLD.refusal_code OR NEW.updated_at!=OLD.updated_at
            )
            AND NOT coalesce((OLD.phase='recovery_required' AND OLD.refusal_code='recovery_proof_failed'
                AND OLD.before_observation IS NULL
                AND OLD.after_observation='quarantined worktree failed its removal-authority proof'
                AND NEW.phase='intent_committed' AND NEW.before_observation IS NULL
                AND NEW.refusal_code IS NULL AND NEW.after_observation='quarantine_reproof_authorized_v1'
                AND EXISTS (SELECT 1 FROM source_worktree_settlement_reproof_items a
                    WHERE a.run_id=OLD.run_id AND a.session_id=OLD.session_id
                      AND a.fenced_at=OLD.updated_at AND a.reopened_at=NEW.updated_at)),0)
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_terminal'); END"#;

const REPROOF_SCHEMA: &str = "
CREATE TABLE source_worktree_settlement_reproof_runs (
    run_id TEXT PRIMARY KEY REFERENCES source_worktree_settlement_runs(run_id),
    attempt_id TEXT NOT NULL UNIQUE CHECK(length(attempt_id)=36 AND attempt_id=lower(attempt_id)
        AND substr(attempt_id,9,1)='-' AND substr(attempt_id,14,1)='-'
        AND substr(attempt_id,19,1)='-' AND substr(attempt_id,24,1)='-'
        AND replace(attempt_id,'-','') NOT GLOB '*[^0-9a-f]*'),
    eligible_count INTEGER NOT NULL CHECK(typeof(eligible_count)='integer' AND eligible_count BETWEEN 1 AND 256),
    state TEXT NOT NULL CHECK(state IN ('intent_committed','applying','partial','settled','refused','recovery_required')),
    settled_count INTEGER NOT NULL DEFAULT 0 CHECK(typeof(settled_count)='integer' AND settled_count BETWEEN 0 AND 256),
    refused_count INTEGER NOT NULL DEFAULT 0 CHECK(typeof(refused_count)='integer' AND refused_count BETWEEN 0 AND 256),
    recovery_required_count INTEGER NOT NULL DEFAULT 0 CHECK(typeof(recovery_required_count)='integer' AND recovery_required_count BETWEEN 0 AND 256),
    unattempted_count INTEGER NOT NULL DEFAULT 0 CHECK(typeof(unattempted_count)='integer' AND unattempted_count BETWEEN 0 AND 256),
    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND created_at GLOB '????-??-??T??:??:??.?????????Z'),
    updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND updated_at GLOB '????-??-??T??:??:??.?????????Z'),
    finished_at TEXT CHECK(finished_at IS NULL OR (length(finished_at)=30 AND finished_at GLOB '????-??-??T??:??:??.?????????Z')),
    CHECK(settled_count+refused_count+recovery_required_count+unattempted_count<=eligible_count)
);
CREATE TABLE source_worktree_settlement_reproof_items (
    run_id TEXT NOT NULL REFERENCES source_worktree_settlement_reproof_runs(run_id),
    session_id TEXT NOT NULL REFERENCES sessions(id),
    fenced_at TEXT NOT NULL CHECK(length(fenced_at)=30 AND fenced_at GLOB '????-??-??T??:??:??.?????????Z'),
    reopened_at TEXT NOT NULL CHECK(length(reopened_at)=30 AND reopened_at GLOB '????-??-??T??:??:??.?????????Z'),
    reason TEXT NOT NULL CHECK(reason='quarantine_first_proof_failed_without_removal_marker'),
    PRIMARY KEY(run_id,session_id)
);
CREATE TRIGGER settlement_reproof_runs_insert BEFORE INSERT ON source_worktree_settlement_reproof_runs
WHEN EXISTS (SELECT 1 FROM source_worktree_settlement_reproof_runs WHERE run_id=NEW.run_id OR attempt_id=NEW.attempt_id)
 OR NEW.state!='intent_committed' OR NEW.finished_at IS NOT NULL
 OR NEW.settled_count+NEW.refused_count+NEW.recovery_required_count+NEW.unattempted_count!=0
 OR NEW.updated_at!=NEW.created_at
 OR NOT EXISTS (SELECT 1 FROM source_worktree_settlement_runs r WHERE r.run_id=NEW.run_id AND r.eligible_count=NEW.eligible_count)
 OR NOT EXISTS (SELECT 1 FROM source_worktree_settlement_items i WHERE i.run_id=NEW.run_id
    AND i.phase='recovery_required' AND i.refusal_code='recovery_proof_failed'
    AND i.before_observation IS NULL
    AND i.after_observation='quarantined worktree failed its removal-authority proof')
BEGIN SELECT RAISE(ABORT,'settlement_reproof_run_invalid'); END;
CREATE TRIGGER settlement_reproof_runs_update BEFORE UPDATE ON source_worktree_settlement_reproof_runs
WHEN NEW.run_id!=OLD.run_id OR NEW.attempt_id!=OLD.attempt_id OR NEW.eligible_count!=OLD.eligible_count
 OR NEW.created_at!=OLD.created_at
 OR NEW.settled_count<OLD.settled_count OR NEW.refused_count<OLD.refused_count
 OR NEW.recovery_required_count<OLD.recovery_required_count OR NEW.unattempted_count<OLD.unattempted_count
 OR ((OLD.finished_at IS NOT NULL OR OLD.state IN ('settled','refused','recovery_required')) AND (NEW.state!=OLD.state OR NEW.updated_at!=OLD.updated_at
    OR NEW.finished_at IS NOT OLD.finished_at OR NEW.settled_count!=OLD.settled_count
    OR NEW.refused_count!=OLD.refused_count OR NEW.recovery_required_count!=OLD.recovery_required_count
    OR NEW.unattempted_count!=OLD.unattempted_count))
 OR (NEW.state!=OLD.state AND NOT (
    (OLD.state='intent_committed' AND NEW.state IN ('applying','partial','settled','refused','recovery_required')) OR
    (OLD.state='applying' AND NEW.state IN ('partial','settled','refused','recovery_required')) OR
    (OLD.state='partial' AND NEW.state IN ('settled','refused','recovery_required'))))
BEGIN SELECT RAISE(ABORT,'settlement_reproof_run_regression'); END;
CREATE TRIGGER settlement_reproof_runs_no_delete BEFORE DELETE ON source_worktree_settlement_reproof_runs
BEGIN SELECT RAISE(ABORT,'settlement_reproof_run_no_delete'); END;
CREATE TRIGGER settlement_reproof_items_insert BEFORE INSERT ON source_worktree_settlement_reproof_items
WHEN EXISTS (SELECT 1 FROM source_worktree_settlement_reproof_items WHERE run_id=NEW.run_id AND session_id=NEW.session_id)
 OR NOT EXISTS (SELECT 1 FROM source_worktree_settlement_reproof_runs r
    JOIN source_worktree_settlement_items i ON i.run_id=r.run_id
    WHERE r.run_id=NEW.run_id AND i.session_id=NEW.session_id
      AND r.created_at=NEW.reopened_at AND r.updated_at=r.created_at AND r.finished_at IS NULL
      AND r.state='intent_committed'
      AND r.settled_count+r.refused_count+r.recovery_required_count+r.unattempted_count=0
      AND i.phase='recovery_required' AND i.refusal_code='recovery_proof_failed'
      AND i.before_observation IS NULL AND i.updated_at=NEW.fenced_at
      AND i.after_observation='quarantined worktree failed its removal-authority proof')
BEGIN SELECT RAISE(ABORT,'settlement_reproof_item_invalid'); END;
CREATE TRIGGER settlement_reproof_items_reopen AFTER INSERT ON source_worktree_settlement_reproof_items
BEGIN
 UPDATE source_worktree_settlement_items SET phase='intent_committed',refusal_code=NULL,
    after_observation='quarantine_reproof_authorized_v1',updated_at=NEW.reopened_at
 WHERE run_id=NEW.run_id AND session_id=NEW.session_id;
END;
CREATE TRIGGER settlement_reproof_items_no_update BEFORE UPDATE ON source_worktree_settlement_reproof_items
BEGIN SELECT RAISE(ABORT,'settlement_reproof_item_immutable'); END;
CREATE TRIGGER settlement_reproof_items_no_delete BEFORE DELETE ON source_worktree_settlement_reproof_items
BEGIN SELECT RAISE(ABORT,'settlement_reproof_item_no_delete'); END;
CREATE VIEW source_worktree_settlement_current_runs AS
SELECT r.run_id,r.schema_version,r.policy_version,r.repository_identity,r.canonical_repo_dir,
 r.target_ref,r.target_oid,r.plan_digest,r.idempotency_key,r.authorization_digest,r.request_fingerprint,
 coalesce(a.state,r.state) AS state,r.observed_count,r.eligible_count,r.retained_count,
 coalesce(a.settled_count,r.settled_count) AS settled_count,
 coalesce(a.refused_count,r.refused_count) AS refused_count,
 coalesce(a.recovery_required_count,r.recovery_required_count) AS recovery_required_count,
 coalesce(a.unattempted_count,r.unattempted_count) AS unattempted_count,
 r.created_at,coalesce(a.updated_at,r.updated_at) AS updated_at,
 CASE WHEN a.run_id IS NULL THEN r.finished_at ELSE a.finished_at END AS finished_at,
 CASE WHEN a.run_id IS NULL THEN r.terminal_error ELSE NULL END AS terminal_error
FROM source_worktree_settlement_runs r LEFT JOIN source_worktree_settlement_reproof_runs a USING(run_id);
";

pub(crate) fn install(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(REPROOF_SCHEMA)?;
    tx.execute_batch(
        "DROP TRIGGER source_worktree_settlement_items_forward_phase;
                      DROP TRIGGER source_worktree_settlement_items_terminal;",
    )?;
    tx.execute_batch(NEW_TRIGGER_0)?;
    tx.execute_batch(NEW_TRIGGER_1)?;
    Ok(())
}

/// Normalize only the exact sanctioned trigger bodies for the retained V95 pin.
/// Any other edit still fails its original fingerprint validation.
pub(super) fn historical_trigger_sql(sql: &str) -> &str {
    match sql {
        NEW_TRIGGER_0 => OLD_TRIGGER_0,
        NEW_TRIGGER_1 => OLD_TRIGGER_1,
        _ => sql,
    }
}
// RSI-RELEASED-MIGRATION-END: v171-settlement-reproof-schema

#[cfg(test)]
pub(crate) fn rewind(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DROP VIEW source_worktree_settlement_current_runs;
        DROP TRIGGER source_worktree_settlement_items_forward_phase;
        DROP TRIGGER source_worktree_settlement_items_terminal;
        DROP TABLE source_worktree_settlement_reproof_items;
        DROP TABLE source_worktree_settlement_reproof_runs;",
    )?;
    tx.execute_batch(OLD_TRIGGER_0)?;
    tx.execute_batch(OLD_TRIGGER_1)?;
    Ok(())
}

impl Store {
    /// Authorize at most one reproof attempt per retained run. This transaction
    /// performs no filesystem effect. The exact old fence is retained in the
    /// immutable audit; original run counters remain untouched forever.
    pub fn reopen_source_worktree_quarantine_proof_fences(&mut self, run_id: Uuid) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates = tx.prepare(
            "SELECT session_id,updated_at FROM source_worktree_settlement_items i
             WHERE run_id=?1 AND phase='recovery_required' AND refusal_code='recovery_proof_failed'
               AND before_observation IS NULL
               AND after_observation='quarantined worktree failed its removal-authority proof'
               AND NOT EXISTS (SELECT 1 FROM source_worktree_settlement_reproof_runs a WHERE a.run_id=i.run_id)
             ORDER BY sequence LIMIT 257"
        )?.query_map([run_id.to_string()], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if candidates.is_empty() {
            return Ok(false);
        }
        if candidates.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS {
            return Err(DaemonError::Store("reproof item bound exceeded".into()));
        }
        let now = timestamp();
        tx.execute(
            "INSERT INTO source_worktree_settlement_reproof_runs
                (run_id,attempt_id,eligible_count,state,created_at,updated_at)
             SELECT run_id,?2,eligible_count,'intent_committed',?3,?3
             FROM source_worktree_settlement_runs WHERE run_id=?1",
            params![run_id.to_string(), Uuid::new_v4().to_string(), now],
        )?;
        for (session_id, fenced_at) in candidates {
            tx.execute(
                "INSERT INTO source_worktree_settlement_reproof_items
                    (run_id,session_id,fenced_at,reopened_at,reason)
                 VALUES (?1,?2,?3,?4,'quarantine_first_proof_failed_without_removal_marker')",
                params![run_id.to_string(), session_id, fenced_at, now],
            )?;
        }
        refresh_run_on(&tx, run_id, &now)?;
        tx.commit()?;
        Ok(true)
    }
}
