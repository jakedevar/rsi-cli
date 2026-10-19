//! Audited reproof of retained intents that never started an effect (#2134).
use super::*;

// RSI-RELEASED-MIGRATION-BEGIN: v173-settlement-untouched-intent
pub(super) const RETAINED_INTENT_FENCE_SQL: &str = "i.phase='recovery_required'
 AND i.refusal_code='recovery_proof_failed' AND i.before_observation IS NULL
 AND i.reserved_effects=0 AND i.active_effects=0
 AND i.after_observation='retained intent followed an earlier terminal stop and cannot prove no prior effect'";

fn insert_trigger(fence: &str) -> String {
    format!("CREATE TRIGGER settlement_completion_retries_insert BEFORE INSERT ON source_worktree_settlement_completion_retries
        WHEN EXISTS (SELECT 1 FROM source_worktree_settlement_completion_retries a
             WHERE a.run_id=NEW.run_id AND a.session_id=NEW.session_id AND a.fenced_at=NEW.fenced_at)
        OR NOT EXISTS (SELECT 1 FROM source_worktree_settlement_items i
             WHERE i.run_id=NEW.run_id AND i.session_id=NEW.session_id
               AND i.updated_at=NEW.fenced_at AND i.refusal_code=NEW.refusal_code
               AND i.after_observation=NEW.observation AND ({fence}))
        BEGIN SELECT RAISE(ABORT,'settlement_completion_retry_invalid'); END")
}

pub(crate) fn install(tx: &Transaction<'_>) -> Result<()> {
    let old: String = tx.query_row(
        "SELECT sql FROM sqlite_master WHERE name='settlement_completion_retries_insert'",
        [],
        |row| row.get(0),
    )?;
    if old != insert_trigger(completion_retry::RETRY_FENCE_SQL) {
        return Err(DaemonError::Store(
            "settlement untouched-intent migration source catalog drifted".into(),
        ));
    }
    tx.execute_batch("DROP TRIGGER settlement_completion_retries_insert;")?;
    tx.execute_batch(&insert_trigger(&format!(
        "({}) OR ({RETAINED_INTENT_FENCE_SQL})",
        completion_retry::RETRY_FENCE_SQL,
    )))?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v173-settlement-untouched-intent

#[cfg(test)]
pub(crate) fn rewind(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("DROP TRIGGER settlement_completion_retries_insert;")?;
    tx.execute_batch(&insert_trigger(completion_retry::RETRY_FENCE_SQL))?;
    Ok(())
}

impl Store {
    /// The caller holds cwd, repository and root exclusion, and has freshly
    /// proved the original root/ref, zero current effects and custody tuple.
    /// Preserve the old fence in the same immutable audit as completion retries.
    pub fn reopen_source_worktree_untouched_intent_fence(
        &mut self,
        run_id: Uuid,
        session_id: Uuid,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            &format!(
                "INSERT INTO source_worktree_settlement_completion_retries
                 (run_id,session_id,fenced_at,reopened_at,refusal_code,observation)
                 SELECT run_id,session_id,updated_at,?3,refusal_code,after_observation
                 FROM source_worktree_settlement_items i
                 WHERE run_id=?1 AND session_id=?2 AND ({RETAINED_INTENT_FENCE_SQL})"
            ),
            params![run_id.to_string(), session_id.to_string(), timestamp()],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }
}
