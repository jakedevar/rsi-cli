//! Exact, daemon-owned continuation of turns interrupted by a graceful restart.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// RSI-RELEASED-MIGRATION-BEGIN: v134-restart-intent-catalog
const TABLE_SQL: &str = "CREATE TABLE daemon_restart_intents (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
    invocation_id TEXT NOT NULL REFERENCES model_invocations(id) ON DELETE RESTRICT,
    custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
    custody_generation INTEGER CHECK(custody_generation IS NULL OR custody_generation>0),
    boot_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(boot_id)),
    claim_boot_id TEXT CHECK(claim_boot_id IS NULL OR rsi_uuid_is_canonical(claim_boot_id)),
    continuation_invocation_id TEXT UNIQUE REFERENCES model_invocations(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK(state IN ('pending','claimed','delivered','failed')),
    outcome TEXT CHECK(outcome IS NULL OR outcome IN ('shutdown_cancelled','restart_reconciled_interrupted','not_needed','review_owned','pending_archive','pending_question','operator_paused','custody_changed','newer_turn','competing_owner','not_resumable','dispatch_failed','dispatch_unknown')),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    claimed_at TEXT CHECK(claimed_at IS NULL OR rsi_rfc3339_nanos_is_canonical(claimed_at)),
    delivered_at TEXT CHECK(delivered_at IS NULL OR rsi_rfc3339_nanos_is_canonical(delivered_at)),
    UNIQUE(session_id,invocation_id),
    CHECK((custody_id IS NULL)=(custody_generation IS NULL)),
    CHECK(state!='delivered' OR continuation_invocation_id IS NOT NULL)
)";
const PENDING_INDEX_SQL: &str = "CREATE INDEX daemon_restart_intents_pending ON daemon_restart_intents(state,id) WHERE state IN ('pending','claimed')";
const SESSION_INDEX_SQL: &str = "CREATE INDEX daemon_restart_intents_session ON daemon_restart_intents(session_id,state,invocation_id)";
const ACTIVE_INDEX_SQL: &str = "CREATE UNIQUE INDEX daemon_restart_intents_one_owner ON daemon_restart_intents(session_id) WHERE state IN ('pending','claimed')";
const REVIEW_FORWARD_SQL: &str = "CREATE TRIGGER manager_review_assignments_v121_forward
        BEFORE UPDATE ON manager_review_assignments
        WHEN NEW.row_version!=OLD.row_version+1 OR NEW.updated_at=OLD.updated_at
          OR NOT ((OLD.state='reserved' AND NEW.state IN ('allocating','superseded','cancelled','failed'))
              OR (OLD.state='allocating' AND NEW.state IN ('active','superseded','cancelled','failed'))
              OR (OLD.state='active' AND NEW.state IN ('submitted','superseded','cancelled','failed'))
              OR (OLD.state IN ('submitted','cancelled','failed') AND NEW.state='superseded')
              OR (OLD.state='active' AND NEW.state='active'
                  AND NEW.reviewer_session_id IS OLD.reviewer_session_id
                  AND NEW.reviewer_custody_id IS OLD.reviewer_custody_id
                  AND NEW.reviewer_custody_generation IS OLD.reviewer_custody_generation
                  AND NEW.action_operation_id IS OLD.action_operation_id
                  AND NEW.failure_code IS OLD.failure_code
                  AND NEW.terminal_at IS OLD.terminal_at
                  AND NEW.superseded_by_assignment_id IS OLD.superseded_by_assignment_id
                  AND NEW.reviewer_invocation_id!=OLD.reviewer_invocation_id
                  AND NOT EXISTS(SELECT 1 FROM manager_review_receipts receipt WHERE receipt.assignment_id=OLD.assignment_id)
                  AND EXISTS(SELECT 1 FROM model_invocations old_invocation
                    WHERE old_invocation.id=OLD.reviewer_invocation_id
                      AND old_invocation.session_id=OLD.reviewer_session_id
                      AND old_invocation.status NOT IN ('running','cancellation_requested'))
                  AND EXISTS(SELECT 1 FROM model_invocations new_invocation
                    JOIN sessions reviewer ON reviewer.id=NEW.reviewer_session_id
                    JOIN sandbox_custody_roots custody ON custody.custody_id=NEW.reviewer_custody_id
                    WHERE new_invocation.id=NEW.reviewer_invocation_id
                      AND new_invocation.session_id=NEW.reviewer_session_id
                      AND new_invocation.parent_invocation_id=OLD.reviewer_invocation_id
                      AND new_invocation.admission_status='admitted'
                      AND new_invocation.status='running'
                      AND new_invocation.purpose='session.continue.resume'
                      AND reviewer.model_invocation_id=NEW.reviewer_invocation_id
                      AND reviewer.sandbox_custody_id=NEW.reviewer_custody_id
                      AND custody.owner_session_id=reviewer.id
                      AND custody.generation=NEW.reviewer_custody_generation
                      AND custody.state='live' AND custody.validation_state='verified'
                      AND custody.validated_generation=custody.generation
                      AND ((new_invocation.trigger_source='daemon_restart'
                            AND EXISTS(SELECT 1 FROM daemon_restart_intents intent
                              WHERE intent.session_id=reviewer.id
                                AND intent.invocation_id=OLD.reviewer_invocation_id
                                AND intent.continuation_invocation_id=new_invocation.id
                                AND intent.state='delivered'
                                AND intent.outcome IN ('shutdown_cancelled','restart_reconciled_interrupted')
                                AND new_invocation.dedup_key='daemon.restart:'||intent.id))
                        OR (new_invocation.trigger_source='continue_session'
                            AND EXISTS(SELECT 1 FROM harness_manager_v2_operations operation
                              WHERE operation.id=substr(new_invocation.dedup_key,16)
                                AND new_invocation.dedup_key='manager.action:'||operation.id
                                AND operation.target_session_id=reviewer.id
                                AND operation.project_id=OLD.project_id
                                AND operation.kind='lifecycle_action'
                                AND json_extract(operation.payload_json,'$.request.operation.action')='resume_lead'
                                AND operation.state IN ('running','succeeded')))))))
        BEGIN SELECT RAISE(ABORT,'manager review assignment transition is not forward'); END";

// Pinned successor for the retained 18 V121 objects and the forward trigger.
const V134_REVIEW_CATALOG_FINGERPRINT: &str =
    "sha256:5963b307e653eb241e87a6cb411f4d4e08f2ef43f50995662d1600e3d5a86948";

pub(crate) const CATALOG: [(&str, &str, &str); 4] = [
    ("table", "daemon_restart_intents", TABLE_SQL),
    ("index", "daemon_restart_intents_pending", PENDING_INDEX_SQL),
    ("index", "daemon_restart_intents_session", SESSION_INDEX_SQL),
    (
        "index",
        "daemon_restart_intents_one_owner",
        ACTIVE_INDEX_SQL,
    ),
];
// RSI-RELEASED-MIGRATION-END: v134-restart-intent-catalog

// RSI-RELEASED-MIGRATION-BEGIN: v134-restart-intent-migration
pub(crate) fn apply_v134_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != 133 {
        return Err(DaemonError::Store(format!(
            "V134 requires V133, found V{version}"
        )));
    }
    super::manager_review_v121::validate_v121_catalog(&tx)?;
    super::topology_v129::validate_v129_catalog(&tx)?;
    super::topology_agent_audit::validate_v133_catalog(&tx)?;
    for (_, _, sql) in CATALOG {
        tx.execute_batch(sql)?;
    }
    // The V121 migration and its released pin stay immutable. Replace only
    // its forward trigger after verifying the exact V121 predecessor.
    tx.execute_batch("DROP TRIGGER manager_review_assignments_v121_forward")?;
    tx.execute_batch(REVIEW_FORWARD_SQL)?;
    tx.execute_batch("PRAGMA user_version = 134")?;
    validate_v134_catalog(&tx)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v134-restart-intent-migration

// RSI-RELEASED-MIGRATION-BEGIN: v134-restart-intent-validator
pub(crate) fn validate_v134_catalog(conn: &Connection) -> Result<()> {
    for (kind, name, expected) in CATALOG {
        let actual: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |r| r.get(0),
            )
            .map_err(|_| DaemonError::Store(format!("V134 catalog missing {name}")))?;
        if actual != expected {
            return Err(DaemonError::Store(format!("V134 catalog changed {name}")));
        }
    }
    let mut hash = Sha256::new();
    for (kind, name) in super::manager_review_v121::V121_CATALOG_OBJECTS {
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |r| r.get(0),
            )
            .map_err(|_| DaemonError::Store(format!("V134 review catalog missing {name}")))?;
        if name == "manager_review_assignments_v121_forward" && sql != REVIEW_FORWARD_SQL {
            return Err(DaemonError::Store(
                "V134 review forward trigger changed".into(),
            ));
        }
        hash.update(kind.as_bytes());
        hash.update([0]);
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(sql.as_bytes());
        hash.update([0xff]);
    }
    let actual = format!("sha256:{:x}", hash.finalize());
    if actual != V134_REVIEW_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V134 review catalog fingerprint mismatch: {actual}"
        )));
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v134-restart-intent-validator

/// Test fixtures that rewind V133 must first remove the V134 successor
/// catalog and restore the exact released V121 trigger text.
#[cfg(test)]
pub(crate) fn rewind_v134_fixture_to_v133(conn: &Connection) {
    conn.execute_batch(
        "DROP TRIGGER manager_review_assignments_v121_forward;
         DROP TABLE daemon_restart_intents;
         CREATE TRIGGER manager_review_assignments_v121_forward
        BEFORE UPDATE ON manager_review_assignments
        WHEN NEW.row_version!=OLD.row_version+1 OR NEW.updated_at=OLD.updated_at
          OR NOT ((OLD.state='reserved' AND NEW.state IN ('allocating','superseded','cancelled','failed'))
              OR (OLD.state='allocating' AND NEW.state IN ('active','superseded','cancelled','failed'))
              OR (OLD.state='active' AND NEW.state IN ('submitted','superseded','cancelled','failed'))
              OR (OLD.state IN ('submitted','cancelled','failed') AND NEW.state='superseded'))
        BEGIN SELECT RAISE(ABORT,'manager review assignment transition is not forward'); END;
         PRAGMA user_version=133;",
    )
    .expect("rewind V134 catalog");
    super::manager_review_v121::validate_v121_catalog(conn).expect("exact V121 trigger restored");
}

#[derive(Debug, Clone)]
pub(crate) struct RestartIntent {
    pub id: Uuid,
    pub session_id: Uuid,
    pub invocation_id: Uuid,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

impl Store {
    /// Called immediately before signaling one still-live turn. The SQL
    /// predicate makes a turn that finished during the drain ineligible.
    pub(crate) fn record_restart_intent(&self, session_id: Uuid, boot_id: Uuid) -> Result<bool> {
        let id = Uuid::new_v4();
        let at = now();
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO daemon_restart_intents
             (id,session_id,invocation_id,custody_id,custody_generation,boot_id,state,created_at,updated_at)
             SELECT ?1,s.id,m.id,p.custody_id,p.custody_generation,?2,'pending',?3,?3
             FROM sessions s JOIN model_invocations m ON m.id=s.model_invocation_id
             LEFT JOIN session_execution_projections p ON p.session_id=s.id
             WHERE s.id=?4 AND (s.status IN ('Starting','Running')
               OR (s.status='WaitingApproval' AND s.pending_question_json IS NULL))
               AND m.session_id=s.id AND m.admission_status='admitted'
               AND m.status IN ('running','cancellation_requested')",
            params![id.to_string(), boot_id.to_string(), at, session_id.to_string()],
        )?;
        if inserted == 1 {
            return Ok(true);
        }
        let existing: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM daemon_restart_intents i JOIN sessions s ON s.id=i.session_id
             WHERE i.session_id=?1 AND i.invocation_id=s.model_invocation_id
               AND i.boot_id=?2 AND i.state='pending')",
            params![session_id.to_string(), boot_id.to_string()],
            |r| r.get(0),
        )?;
        Ok(existing)
    }

    pub(crate) fn mark_restart_interrupt_sent(
        &self,
        session_id: Uuid,
        boot_id: Uuid,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE daemon_restart_intents SET outcome='shutdown_cancelled',updated_at=?3
             WHERE session_id=?1 AND boot_id=?2 AND state='pending'",
            params![session_id.to_string(), boot_id.to_string(), now()],
        )?;
        Ok(())
    }

    /// Before generic invocation reconciliation, mark only the exact
    /// gracefully interrupted invocation as Interrupted. The existing model
    /// reconciler then records `restart_reconciled_interrupted`.
    pub(crate) fn prepare_restart_intents_for_restore(&self) -> Result<usize> {
        let mut after = String::new();
        let mut count = 0;
        loop {
            let mut stmt = self.conn.prepare(
                "SELECT id,session_id,invocation_id FROM daemon_restart_intents
                 WHERE state IN ('pending','claimed') AND id>?1 ORDER BY id LIMIT 128",
            )?;
            let page: Vec<(String, String, String)> = stmt
                .query_map([&after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            if page.is_empty() {
                break;
            }
            for (id, session, invocation) in &page {
                let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
                let at = now();
                let changed = tx.execute(
                    "UPDATE sessions SET status='Interrupted',
                     stop_reason=COALESCE(NULLIF(TRIM(stop_reason),''),'interrupted:daemon_restart'),
                     updated_at=?1
                     WHERE id=?2 AND model_invocation_id=?3
                       AND (status IN ('Starting','Running') OR (status='WaitingApproval' AND pending_question_json IS NULL))
                       AND EXISTS(SELECT 1 FROM model_invocations WHERE id=?3 AND session_id=?2
                         AND admission_status='admitted' AND status IN ('running','cancellation_requested'))",
                    params![at, session, invocation],
                )?;
                if changed == 1 {
                    count += 1;
                }
                let finished: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions s JOIN model_invocations m ON m.id=?2
                     WHERE s.id=?1 AND s.model_invocation_id=?2
                       AND s.status IN ('Completed','Archived') AND m.status NOT IN ('running','cancellation_requested'))",
                    params![session, invocation], |r| r.get(0),
                )?;
                if finished {
                    tx.execute("UPDATE daemon_restart_intents SET state='failed',outcome='not_needed',updated_at=?2 WHERE id=?1 AND state IN ('pending','claimed')", params![id, at])?;
                } else if changed == 1 {
                    tx.execute("UPDATE daemon_restart_intents SET outcome='restart_reconciled_interrupted',updated_at=?2 WHERE id=?1", params![id, at])?;
                } else {
                    tx.execute(
                        "UPDATE daemon_restart_intents SET state='failed',outcome='dispatch_unknown',updated_at=?2
                         WHERE id=?1 AND state IN ('pending','claimed')
                           AND EXISTS(SELECT 1 FROM sessions s JOIN model_invocations m ON m.id=?3
                             WHERE s.id=?4 AND s.model_invocation_id=?3
                               AND s.status IN ('Starting','Running')
                               AND m.status NOT IN ('running','cancellation_requested'))",
                        params![id, at, invocation, session],
                    )?;
                }
                tx.commit()?;
            }
            if let Some(last) = page.last() {
                after.clone_from(&last.0);
            }
        }
        Ok(count)
    }

    pub(crate) fn restart_intent_owns_session(&self, session: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM daemon_restart_intents i JOIN sessions s ON s.id=i.session_id
             WHERE i.session_id=?1 AND i.invocation_id=s.model_invocation_id
               AND i.state IN ('pending','claimed','delivered'))",
            [session.to_string()], |r| r.get(0),
        )?)
    }

    pub(crate) fn restart_owned_session_ids(&self) -> Result<std::collections::HashSet<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.session_id FROM daemon_restart_intents i JOIN sessions s ON s.id=i.session_id
             WHERE i.invocation_id=s.model_invocation_id AND i.state IN ('pending','claimed','delivered')",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|id| Uuid::parse_str(&id?).map_err(|e| DaemonError::Store(e.to_string())))
            .collect()
    }

    pub(crate) fn next_restart_intent(&self, after: Option<Uuid>) -> Result<Option<RestartIntent>> {
        let row: Option<(String,String,String)> = self.conn.query_row(
            "SELECT i.id,i.session_id,i.invocation_id FROM daemon_restart_intents i
             WHERE i.state IN ('pending','claimed') AND i.id>?1
               AND NOT EXISTS(SELECT 1 FROM manager_review_assignments a
                 WHERE a.reviewer_session_id=i.session_id AND a.reviewer_invocation_id=i.invocation_id
                   AND a.state IN ('reserved','allocating','active'))
             ORDER BY i.id LIMIT 1",
            [after.map(|id| id.to_string()).unwrap_or_default()],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).optional()?;
        row.map(|(id, session_id, invocation_id)| {
            Ok(RestartIntent {
                id: Uuid::parse_str(&id).map_err(|e| DaemonError::Store(e.to_string()))?,
                session_id: Uuid::parse_str(&session_id)
                    .map_err(|e| DaemonError::Store(e.to_string()))?,
                invocation_id: Uuid::parse_str(&invocation_id)
                    .map_err(|e| DaemonError::Store(e.to_string()))?,
            })
        })
        .transpose()
    }

    /// The review recovery pass claims its own journal rows before the
    /// ordinary restart pass. The assignment remains bound to the old
    /// invocation until the admitted continuation is proven and rebound.
    pub(crate) fn next_review_restart_intent(
        &self,
        after: Option<Uuid>,
    ) -> Result<Option<RestartIntent>> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT i.id,i.session_id,i.invocation_id FROM daemon_restart_intents i
                 JOIN manager_review_assignments a
                   ON a.reviewer_session_id=i.session_id
                  AND a.reviewer_invocation_id=i.invocation_id AND a.state='active'
                 WHERE i.state IN ('pending','claimed') AND i.id>?1
                 ORDER BY i.id LIMIT 1",
                [after.map(|id| id.to_string()).unwrap_or_default()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(id, session_id, invocation_id)| {
            Ok(RestartIntent {
                id: Uuid::parse_str(&id).map_err(|e| DaemonError::Store(e.to_string()))?,
                session_id: Uuid::parse_str(&session_id)
                    .map_err(|e| DaemonError::Store(e.to_string()))?,
                invocation_id: Uuid::parse_str(&invocation_id)
                    .map_err(|e| DaemonError::Store(e.to_string()))?,
            })
        })
        .transpose()
    }

    pub(crate) fn claim_restart_intent(
        &self,
        intent: &RestartIntent,
        boot_id: Uuid,
    ) -> Result<bool> {
        self.claim_restart_intent_with_review(intent, boot_id, false)
    }

    pub(crate) fn claim_review_restart_intent(
        &self,
        intent: &RestartIntent,
        boot_id: Uuid,
    ) -> Result<bool> {
        self.claim_restart_intent_with_review(intent, boot_id, true)
    }

    fn claim_restart_intent_with_review(
        &self,
        intent: &RestartIntent,
        boot_id: Uuid,
        review_owned: bool,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let at = now();
        let reason: Option<String> = tx.query_row(
            "SELECT CASE
               WHEN s.model_invocation_id IS NOT i.invocation_id THEN 'newer_turn'
               WHEN i.continuation_invocation_id IS NOT NULL THEN 'dispatch_unknown'
               WHEN s.pending_archive=1 THEN 'pending_archive'
               WHEN s.pending_question_json IS NOT NULL THEN 'pending_question'
               WHEN EXISTS(SELECT 1 FROM daemon_settings WHERE key='manager_operator_pause:'||s.id AND value<>'false') THEN 'operator_paused'
               WHEN ?4=1 AND NOT EXISTS(SELECT 1 FROM manager_review_assignments a
                 WHERE a.reviewer_session_id=s.id AND a.reviewer_invocation_id=i.invocation_id
                   AND a.state='active') THEN 'review_owned'
               WHEN ?4=0 AND EXISTS(SELECT 1 FROM manager_review_assignments a
                 WHERE a.reviewer_session_id=s.id AND a.reviewer_invocation_id=i.invocation_id)
                 THEN 'review_owned'
               WHEN s.status!='Interrupted' OR (COALESCE(m.error_class,'')!='restart_reconciled_interrupted' AND COALESCE(i.outcome,'') NOT IN ('shutdown_cancelled','restart_reconciled_interrupted')) THEN 'competing_owner'
               WHEN p.execution_state='ordinary_unsandboxed' AND p.freshness='verified' AND i.custody_id IS NULL THEN NULL
               WHEN p.execution_state='live_sandboxed' AND p.freshness='verified'
                 AND p.custody_id=i.custody_id AND p.custody_generation=i.custody_generation
                 AND r.state='live' AND r.validation_state='verified' AND r.owner_session_id=s.id
                 AND r.generation=i.custody_generation THEN NULL
               ELSE 'custody_changed' END
             FROM daemon_restart_intents i JOIN sessions s ON s.id=i.session_id
             JOIN model_invocations m ON m.id=i.invocation_id
             LEFT JOIN session_execution_projections p ON p.session_id=s.id
             LEFT JOIN sandbox_custody_roots r ON r.custody_id=p.custody_id
             WHERE i.id=?1 AND i.session_id=?2 AND i.invocation_id=?3 AND i.state IN ('pending','claimed')",
            params![intent.id.to_string(),intent.session_id.to_string(),intent.invocation_id.to_string(),review_owned],
            |r| r.get(0),
        ).optional()?.flatten();
        if let Some(reason) = reason {
            let state = if reason == "review_owned"
                || reason == "pending_archive"
                || reason == "newer_turn"
                || reason == "dispatch_unknown"
            {
                "failed"
            } else {
                "pending"
            };
            tx.execute(
                "UPDATE daemon_restart_intents SET state=?2,outcome=?3,updated_at=?4 WHERE id=?1",
                params![intent.id.to_string(), state, reason, at],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        let changed = tx.execute(
            "UPDATE daemon_restart_intents SET state='claimed',claim_boot_id=?2,claimed_at=?3,updated_at=?3
             WHERE id=?1 AND state IN ('pending','claimed') AND continuation_invocation_id IS NULL",
            params![intent.id.to_string(),boot_id.to_string(),at],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub(crate) fn bind_restart_continuation(
        &self,
        intent_id: Uuid,
        invocation_id: Uuid,
        boot_id: Uuid,
    ) -> Result<()> {
        let at = now();
        let changed = self.conn.execute(
            "UPDATE daemon_restart_intents SET state='delivered',continuation_invocation_id=?2,delivered_at=?4,updated_at=?4
             WHERE id=?1 AND state='claimed' AND claim_boot_id=?3 AND continuation_invocation_id IS NULL
               AND EXISTS(SELECT 1 FROM model_invocations m WHERE m.id=?2 AND m.session_id=daemon_restart_intents.session_id
                 AND m.parent_invocation_id=daemon_restart_intents.invocation_id AND m.admission_status='admitted'
                 AND m.dedup_key='daemon.restart:'||daemon_restart_intents.id)",
            params![intent_id.to_string(),invocation_id.to_string(),boot_id.to_string(),at],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "restart continuation ownership changed".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn fail_restart_intent(&self, intent_id: Uuid, outcome: &str) -> Result<()> {
        if !matches!(outcome, "not_resumable" | "dispatch_failed") {
            return Err(DaemonError::Store("invalid restart intent outcome".into()));
        }
        self.conn.execute("UPDATE daemon_restart_intents SET state='failed',outcome=?2,updated_at=?3 WHERE id=?1 AND state IN ('claimed','delivered')", params![intent_id.to_string(),outcome,now()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::SessionStatus;

    fn session_with_invocation(store: &Store) -> (Uuid, Uuid) {
        let at = now();
        let mut session = crate::store::tests::make_test_session();
        session.id = Uuid::new_v4();
        session.project_id = None;
        session.status = SessionStatus::Running;
        store.insert_session(&session).expect("session");
        let invocation = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
             (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
              trigger_source,session_id,created_at)
             VALUES(?1,'session.launch','session','foreground','paid','admitted','running',
                    'launch_session',?2,?3)",
                params![invocation.to_string(), session.id.to_string(), at],
            )
            .expect("invocation");
        store
            .set_session_model_invocation(session.id, Some(invocation))
            .expect("bind invocation");
        store
            .conn
            .execute(
                "INSERT INTO session_execution_projections
             (session_id,schema_version,projection_version,execution_state,freshness,
              canonical_repo_dir,effective_cwd,validated_at,updated_at)
             VALUES(?1,1,1,'ordinary_unsandboxed','verified',?2,?2,?3,?3)
             ON CONFLICT(session_id) DO UPDATE SET execution_state='ordinary_unsandboxed',
               freshness='verified',effective_cwd=excluded.effective_cwd,
               validated_at=excluded.validated_at,updated_at=excluded.updated_at",
                params![
                    session.id.to_string(),
                    session.working_dir.to_str().expect("UTF-8 fixture path"),
                    at
                ],
            )
            .expect("ordinary custody projection");
        (session.id, invocation)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn v134_reopens_with_successor_review_catalog() {
        let directory = tempfile::tempdir().expect("database directory");
        let path = directory.path().join("restart.sqlite");
        let store = Store::open(&path).expect("migrate to current head");
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, super::super::LATEST_SCHEMA_VERSION);
        super::super::tests::rewind_post_v121_tail_to(&store.conn, 134);
        validate_v134_catalog(&store.conn).expect("authenticate V134 predecessor catalog");
        drop(store);
        let reopened = Store::open(&path).expect("reopen pinned successor catalog");
        validate_v134_catalog(&reopened.conn).expect("authenticate V134 catalog at head");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn exact_restart_owner_excludes_finished_and_crash_only_turns() {
        let directory = tempfile::tempdir().expect("database directory");
        let store = Store::open(&directory.path().join("restart.sqlite")).expect("store");
        let boot = Uuid::new_v4();
        let next_boot = Uuid::new_v4();
        let (session, invocation) = session_with_invocation(&store);
        assert!(store.record_restart_intent(session, boot).expect("record"));
        assert!(
            store
                .record_restart_intent(session, boot)
                .expect("idempotent record")
        );
        store
            .mark_restart_interrupt_sent(session, boot)
            .expect("interrupted");
        assert_eq!(
            store
                .prepare_restart_intents_for_restore()
                .expect("prepare"),
            1
        );
        let interrupted = store.get_session(session).unwrap().unwrap();
        assert_eq!(interrupted.status, SessionStatus::Interrupted);
        assert_eq!(
            interrupted.stop_reason.as_deref(),
            Some("interrupted:daemon_restart"),
            "#588: the restart reconciliation records why the turn was interrupted"
        );
        store.conn.execute(
            "UPDATE model_invocations SET status='failed',error_class='restart_reconciled_interrupted' WHERE id=?1",
            [invocation.to_string()],
        ).expect("reconcile exact invocation");
        let intent = store.next_restart_intent(None).unwrap().expect("intent");
        assert_eq!(
            (intent.session_id, intent.invocation_id),
            (session, invocation)
        );
        assert!(
            store
                .claim_restart_intent(&intent, next_boot)
                .expect("claim")
        );
        let continuation = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
             (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
              trigger_source,session_id,parent_invocation_id,dedup_key,created_at)
             VALUES(?1,'session.continue.resume','session','foreground','paid','admitted','running',
                    'daemon_restart',?2,?3,?4,?5)",
                params![
                    continuation.to_string(),
                    session.to_string(),
                    invocation.to_string(),
                    format!("daemon.restart:{}", intent.id),
                    now()
                ],
            )
            .expect("continuation admission");
        store
            .bind_restart_continuation(intent.id, continuation, next_boot)
            .expect("bind exact continuation");
        assert!(store.next_restart_intent(None).unwrap().is_none());
        assert!(
            !store
                .claim_restart_intent(&intent, Uuid::new_v4())
                .expect("no duplicate")
        );

        let (finished, finished_invocation) = session_with_invocation(&store);
        store
            .record_restart_intent(finished, boot)
            .expect("finished intent");
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [finished.to_string()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='completed' WHERE id=?1",
                [finished_invocation.to_string()],
            )
            .unwrap();
        assert_eq!(store.prepare_restart_intents_for_restore().unwrap(), 0);
        let outcome: String = store
            .conn
            .query_row(
                "SELECT outcome FROM daemon_restart_intents WHERE session_id=?1",
                [finished.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(outcome, "not_needed");

        let (crash_only, _) = session_with_invocation(&store);
        assert_eq!(store.prepare_restart_intents_for_restore().unwrap(), 0);
        assert_eq!(
            store.get_session(crash_only).unwrap().unwrap().status,
            SessionStatus::Running
        );
    }
}
