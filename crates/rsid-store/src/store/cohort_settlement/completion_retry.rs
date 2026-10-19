//! Audited retries of interrupted settlement completion (#2111).
use super::*;

// RSI-RELEASED-MIGRATION-BEGIN: v172-settlement-completion-retry
pub(super) const RETRY_FENCE_SQL: &str = "i.phase='recovery_required' AND (
 (i.refusal_code='recovery_proof_failed' AND
  i.after_observation='quarantined worktree failed its removal-authority proof' AND
  (i.before_observation IS NOT NULL OR EXISTS (SELECT 1 FROM source_worktree_settlement_reproof_runs a WHERE a.run_id=i.run_id)))
 OR (i.before_observation IS NOT NULL AND i.refusal_code IN ('ref_delete_failed','worktree_residue'))
 OR (i.refusal_code='recovery_proof_failed' AND
     i.after_observation='settlement attempt failed: sandbox_custody:root_busy'))";
const OLD_TRIGGER_0: &str = r#"CREATE TRIGGER source_worktree_settlement_items_forward_phase BEFORE UPDATE ON source_worktree_settlement_items
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
            AND NOT coalesce((OLD.phase='recovery_required' AND OLD.refusal_code='recovery_proof_failed'
                AND OLD.before_observation IS NULL
                AND OLD.after_observation='quarantined worktree failed its removal-authority proof'
                AND NEW.phase='intent_committed' AND NEW.before_observation IS NULL
                AND NEW.refusal_code IS NULL AND NEW.after_observation='quarantine_reproof_authorized_v1'
                AND EXISTS (SELECT 1 FROM source_worktree_settlement_reproof_items a
                    WHERE a.run_id=OLD.run_id AND a.session_id=OLD.session_id
                      AND a.fenced_at=OLD.updated_at AND a.reopened_at=NEW.updated_at)),0)
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_terminal'); END"#;
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
            AND NOT coalesce((OLD.phase='recovery_required' AND NEW.phase='intent_committed'
                AND NEW.before_observation IS OLD.before_observation
                AND NEW.refusal_code IS NULL AND NEW.after_observation='settlement_completion_retry_v1'
                AND EXISTS (SELECT 1 FROM source_worktree_settlement_completion_retries a
                    WHERE a.run_id=OLD.run_id AND a.session_id=OLD.session_id
                      AND a.fenced_at=OLD.updated_at AND a.reopened_at=NEW.updated_at
                      AND a.refusal_code=OLD.refusal_code AND a.observation=OLD.after_observation)),0)
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_phase_regression'); END"#;
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
            AND NOT coalesce((OLD.phase='recovery_required' AND NEW.phase='intent_committed'
                AND NEW.before_observation IS OLD.before_observation
                AND NEW.refusal_code IS NULL AND NEW.after_observation='settlement_completion_retry_v1'
                AND EXISTS (SELECT 1 FROM source_worktree_settlement_completion_retries a
                    WHERE a.run_id=OLD.run_id AND a.session_id=OLD.session_id
                      AND a.fenced_at=OLD.updated_at AND a.reopened_at=NEW.updated_at
                      AND a.refusal_code=OLD.refusal_code AND a.observation=OLD.after_observation)),0)
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_terminal'); END"#;
const OLD_VIEW: &str = r#"CREATE VIEW source_worktree_settlement_current_runs AS
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
FROM source_worktree_settlement_runs r LEFT JOIN source_worktree_settlement_reproof_runs a USING(run_id)"#;
const COMPLETION_VIEW_SQL: &str = r#"DROP VIEW source_worktree_settlement_current_runs;
    CREATE VIEW source_worktree_settlement_current_runs AS
    WITH previous AS (SELECT r.run_id,r.schema_version,r.policy_version,r.repository_identity,r.canonical_repo_dir,
 r.target_ref,r.target_oid,r.plan_digest,r.idempotency_key,r.authorization_digest,r.request_fingerprint,
 coalesce(a.state,r.state) AS state,r.observed_count,r.eligible_count,r.retained_count,
 coalesce(a.settled_count,r.settled_count) AS settled_count,
 coalesce(a.refused_count,r.refused_count) AS refused_count,
 coalesce(a.recovery_required_count,r.recovery_required_count) AS recovery_required_count,
 coalesce(a.unattempted_count,r.unattempted_count) AS unattempted_count,
 r.created_at,coalesce(a.updated_at,r.updated_at) AS updated_at,
 CASE WHEN a.run_id IS NULL THEN r.finished_at ELSE a.finished_at END AS finished_at,
 CASE WHEN a.run_id IS NULL THEN r.terminal_error ELSE NULL END AS terminal_error
FROM source_worktree_settlement_runs r LEFT JOIN source_worktree_settlement_reproof_runs a USING(run_id)), counts AS (
      SELECT i.run_id,count(*) AS eligible,sum(i.phase='settled') AS settled,
       sum(i.phase='refused') AS refused,sum(i.phase='recovery_required') AS recovery,
       sum(i.phase='unattempted') AS unattempted,sum(i.phase='intent_committed') AS intent,
       sum(i.phase IN ('settled','refused','recovery_required','unattempted')) AS terminal,
       max(i.updated_at) AS updated_at
      FROM source_worktree_settlement_items i
      WHERE EXISTS (SELECT 1 FROM source_worktree_settlement_completion_retries a WHERE a.run_id=i.run_id)
      GROUP BY i.run_id
    )
    SELECT r.run_id,r.schema_version,r.policy_version,r.repository_identity,r.canonical_repo_dir,
     r.target_ref,r.target_oid,r.plan_digest,r.idempotency_key,r.authorization_digest,r.request_fingerprint,
     CASE WHEN c.run_id IS NULL THEN r.state
       WHEN c.terminal=c.eligible AND c.recovery>0 THEN 'recovery_required'
       WHEN c.settled=c.eligible THEN 'settled'
       WHEN c.terminal=c.eligible AND c.settled=0 THEN 'refused'
       WHEN c.terminal>0 THEN 'partial' WHEN c.intent=c.eligible THEN 'intent_committed' ELSE 'applying' END AS state,
     r.observed_count,r.eligible_count,r.retained_count,
     coalesce(c.settled,r.settled_count) AS settled_count,coalesce(c.refused,r.refused_count) AS refused_count,
     coalesce(c.recovery,r.recovery_required_count) AS recovery_required_count,
     coalesce(c.unattempted,r.unattempted_count) AS unattempted_count,r.created_at,
     coalesce(c.updated_at,r.updated_at) AS updated_at,
     CASE WHEN c.run_id IS NULL THEN r.finished_at WHEN c.terminal=c.eligible THEN c.updated_at END AS finished_at,
     CASE WHEN c.run_id IS NULL THEN r.terminal_error END AS terminal_error
    FROM previous r LEFT JOIN counts c USING(run_id);"#;

pub(crate) fn install(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE source_worktree_settlement_completion_retries (
        run_id TEXT NOT NULL,
        session_id TEXT NOT NULL,
        fenced_at TEXT NOT NULL CHECK(length(fenced_at)=30 AND fenced_at GLOB '????-??-??T??:??:??.?????????Z'),
        reopened_at TEXT NOT NULL CHECK(length(reopened_at)=30 AND reopened_at GLOB '????-??-??T??:??:??.?????????Z'),
        refusal_code TEXT NOT NULL,
        observation TEXT NOT NULL CHECK(octet_length(observation)<=16384),
        PRIMARY KEY(run_id,session_id,fenced_at),
        FOREIGN KEY(run_id,session_id) REFERENCES source_worktree_settlement_items(run_id,session_id)
    );
    CREATE TRIGGER settlement_completion_retries_no_update BEFORE UPDATE ON source_worktree_settlement_completion_retries
    BEGIN SELECT RAISE(ABORT,'settlement_completion_retry_immutable'); END;
    CREATE TRIGGER settlement_completion_retries_no_delete BEFORE DELETE ON source_worktree_settlement_completion_retries
    BEGIN SELECT RAISE(ABORT,'settlement_completion_retry_no_delete'); END;")?;
    tx.execute_batch(&format!("CREATE TRIGGER settlement_completion_retries_insert BEFORE INSERT ON source_worktree_settlement_completion_retries
        WHEN EXISTS (SELECT 1 FROM source_worktree_settlement_completion_retries a
             WHERE a.run_id=NEW.run_id AND a.session_id=NEW.session_id AND a.fenced_at=NEW.fenced_at)
        OR NOT EXISTS (SELECT 1 FROM source_worktree_settlement_items i
             WHERE i.run_id=NEW.run_id AND i.session_id=NEW.session_id
               AND i.updated_at=NEW.fenced_at AND i.refusal_code=NEW.refusal_code
               AND i.after_observation=NEW.observation AND ({RETRY_FENCE_SQL}))
        BEGIN SELECT RAISE(ABORT,'settlement_completion_retry_invalid'); END;
        CREATE TRIGGER settlement_completion_retries_reopen AFTER INSERT ON source_worktree_settlement_completion_retries
        BEGIN UPDATE source_worktree_settlement_items SET phase='intent_committed',refusal_code=NULL,
             after_observation='settlement_completion_retry_v1',updated_at=NEW.reopened_at
             WHERE run_id=NEW.run_id AND session_id=NEW.session_id; END;"))?;
    for (name, expected) in [
        (
            "source_worktree_settlement_items_forward_phase",
            OLD_TRIGGER_0,
        ),
        ("source_worktree_settlement_items_terminal", OLD_TRIGGER_1),
        ("source_worktree_settlement_current_runs", OLD_VIEW),
    ] {
        let old: String =
            tx.query_row("SELECT sql FROM sqlite_master WHERE name=?1", [name], |r| {
                r.get(0)
            })?;
        if old != expected {
            return Err(DaemonError::Store(
                "settlement completion migration source catalog drifted".into(),
            ));
        }
    }
    tx.execute_batch(
        "DROP TRIGGER source_worktree_settlement_items_forward_phase;
        DROP TRIGGER source_worktree_settlement_items_terminal;",
    )?;
    tx.execute_batch(NEW_TRIGGER_0)?;
    tx.execute_batch(NEW_TRIGGER_1)?;
    tx.execute_batch(COMPLETION_VIEW_SQL)?;
    Ok(())
}

pub(super) fn historical_trigger_sql(sql: &str) -> String {
    match sql {
        NEW_TRIGGER_0 => OLD_TRIGGER_0.to_owned(),
        NEW_TRIGGER_1 => OLD_TRIGGER_1.to_owned(),
        _ => sql.to_owned(),
    }
}
// RSI-RELEASED-MIGRATION-END: v172-settlement-completion-retry

#[cfg(test)]
pub(crate) fn rewind(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DROP VIEW source_worktree_settlement_current_runs;
        DROP TRIGGER source_worktree_settlement_items_forward_phase;
        DROP TRIGGER source_worktree_settlement_items_terminal;
        DROP TABLE source_worktree_settlement_completion_retries;",
    )?;
    tx.execute_batch(OLD_TRIGGER_0)?;
    tx.execute_batch(OLD_TRIGGER_1)?;
    tx.execute_batch(OLD_VIEW)?;
    Ok(())
}

impl Store {
    /// Retain the completion ordering across proof deferrals and restarts.
    /// The immutable audit, rather than a mutable observation label, selects it.
    pub fn source_worktree_completion_retry_exists(
        &self,
        run_id: Uuid,
        session_id: Uuid,
    ) -> Result<bool> {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM source_worktree_settlement_completion_retries WHERE run_id=?1 AND session_id=?2)",
            params![run_id.to_string(), session_id.to_string()], |row| row.get(0)).map_err(Into::into)
    }

    /// Append a bounded recovery attempt; the preceding fence and run counters
    /// remain durable history. No filesystem effect or holder waiver occurs here.
    pub fn reopen_source_worktree_completion_fences(&mut self, run_id: Uuid) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        let changed = tx.execute(
            &format!(
                "INSERT INTO source_worktree_settlement_completion_retries
          (run_id,session_id,fenced_at,reopened_at,refusal_code,observation)
          SELECT run_id,session_id,updated_at,?2,refusal_code,after_observation
          FROM source_worktree_settlement_items i WHERE run_id=?1 AND ({RETRY_FENCE_SQL})
          ORDER BY sequence LIMIT 256"
            ),
            params![run_id.to_string(), now],
        )?;
        tx.commit()?;
        Ok(changed > 0)
    }
}
