//! Durable, append-only per-request journal for same-row agent child relaunch.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rsi_common::agent_coordination::{
    AgentContinuationCursorV1, AgentContinueChildResultV1, AgentContinueRelaunchV1,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

// RSI-RELEASED-MIGRATION-BEGIN: v130-agent-child-relaunch-intents
pub(super) fn apply_v130_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 129 {
        return Err(DaemonError::Store(format!(
            "V130 requires exact V129 source, found V{version}"
        )));
    }
    tx.execute_batch(
        "CREATE TABLE agent_child_relaunch_intents (
            request_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(request_id)),
            key_digest TEXT NOT NULL UNIQUE CHECK(rsi_sha256_digest_is_canonical(key_digest)),
            request_fingerprint TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(request_fingerprint)),
            caller_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(caller_session_id)),
            target_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(target_session_id)),
            tip_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(tip_session_id)),
            observed_event_sequence INTEGER NOT NULL CHECK(observed_event_sequence>=0),
            observed_custody_generation INTEGER CHECK(observed_custody_generation IS NULL OR observed_custody_generation>0),
            dedup_key TEXT NOT NULL UNIQUE CHECK(dedup_key='agent.child_relaunch.v1:'||request_id),
            state TEXT NOT NULL CHECK(state IN ('intent','launched','abandoned')),
            invocation_id TEXT REFERENCES model_invocations(id) ON DELETE RESTRICT CHECK(invocation_id IS NULL OR rsi_uuid_is_canonical(invocation_id)),
            receipt_json TEXT CHECK(receipt_json IS NULL OR (json_valid(receipt_json) AND length(receipt_json)<=4096)),
            abandon_reason TEXT CHECK(abandon_reason IS NULL OR abandon_reason IN ('no_launch_evidence','admission_refused','spawn_failed','task_unresolved')),
            created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
            settled_at TEXT CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
            CHECK((state='intent' AND receipt_json IS NULL AND settled_at IS NULL AND abandon_reason IS NULL)
               OR (state='launched' AND invocation_id IS NOT NULL AND receipt_json IS NOT NULL AND settled_at IS NOT NULL AND abandon_reason IS NULL)
               OR (state='abandoned' AND abandon_reason IS NOT NULL AND receipt_json IS NOT NULL AND settled_at IS NOT NULL))
        );
        CREATE UNIQUE INDEX agent_child_relaunch_intents_one_open_per_tip
            ON agent_child_relaunch_intents(tip_session_id) WHERE state='intent';
        CREATE TRIGGER agent_child_relaunch_intents_no_delete
        BEFORE DELETE ON agent_child_relaunch_intents BEGIN
            SELECT RAISE(ABORT,'agent_child_relaunch_intents_append_only');
        END;
        CREATE TRIGGER agent_child_relaunch_intents_forward
        BEFORE UPDATE ON agent_child_relaunch_intents
        WHEN OLD.state!='intent'
          OR NEW.request_id!=OLD.request_id OR NEW.key_digest!=OLD.key_digest
          OR NEW.request_fingerprint!=OLD.request_fingerprint
          OR NEW.caller_session_id!=OLD.caller_session_id
          OR NEW.target_session_id!=OLD.target_session_id
          OR NEW.tip_session_id!=OLD.tip_session_id
          OR NEW.observed_event_sequence!=OLD.observed_event_sequence
          OR NEW.observed_custody_generation IS NOT OLD.observed_custody_generation
          OR NEW.dedup_key!=OLD.dedup_key OR NEW.created_at!=OLD.created_at
          OR (OLD.invocation_id IS NOT NULL AND NEW.invocation_id IS NOT OLD.invocation_id)
          OR NOT (
              (NEW.state='intent' AND OLD.invocation_id IS NULL AND NEW.invocation_id IS NOT NULL
               AND NEW.receipt_json IS OLD.receipt_json AND NEW.settled_at IS OLD.settled_at
               AND NEW.abandon_reason IS OLD.abandon_reason)
              OR NEW.state IN ('launched','abandoned'))
        BEGIN SELECT RAISE(ABORT,'agent_child_relaunch_intents_append_only'); END;
        PRAGMA user_version = 130;",
    )?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v130-agent-child-relaunch-intents

#[cfg(test)]
pub(crate) const V130_CATALOG_OBJECTS: [(&str, &str); 4] = [
    ("table", "agent_child_relaunch_intents"),
    ("index", "agent_child_relaunch_intents_one_open_per_tip"),
    ("trigger", "agent_child_relaunch_intents_no_delete"),
    ("trigger", "agent_child_relaunch_intents_forward"),
];

#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) fn rewind_v130_fixture_to_v129(connection: &rusqlite::Connection) {
    let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .expect("begin V130 child relaunch rewind");
    let version: i32 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read version before V130 child relaunch rewind");
    assert_eq!(version, 130, "V130 child relaunch rewind requires V130");
    tx.execute_batch(
        "DROP TRIGGER agent_child_relaunch_intents_forward;
         DROP TRIGGER agent_child_relaunch_intents_no_delete;
         DROP INDEX agent_child_relaunch_intents_one_open_per_tip;
         DROP TABLE agent_child_relaunch_intents;
         PRAGMA user_version=129;",
    )
    .expect("remove exact V130 child relaunch catalog");
    tx.commit().expect("commit V130 child relaunch rewind");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelaunchState {
    Intent,
    Launched,
    Abandoned,
}

#[derive(Debug, Clone)]
pub(crate) struct RelaunchIntentRow {
    pub request_id: Uuid,
    pub key_digest: String,
    pub request_fingerprint: String,
    pub caller_session_id: Uuid,
    pub target_session_id: Uuid,
    pub tip_session_id: Uuid,
    pub observed_event_sequence: i64,
    pub observed_custody_generation: Option<i64>,
    pub dedup_key: String,
    pub state: RelaunchState,
    pub invocation_id: Option<Uuid>,
    pub receipt_json: Option<String>,
    pub abandon_reason: Option<String>,
}

pub(crate) enum InsertRelaunchIntent {
    Inserted,
    Existing(Box<RelaunchIntentRow>),
    TipInProgress,
}

fn parse_uuid(value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RelaunchIntentRow> {
    let state: String = row.get(9)?;
    Ok(RelaunchIntentRow {
        request_id: parse_uuid(&row.get::<_, String>(0)?)?,
        key_digest: row.get(1)?,
        request_fingerprint: row.get(2)?,
        caller_session_id: parse_uuid(&row.get::<_, String>(3)?)?,
        target_session_id: parse_uuid(&row.get::<_, String>(4)?)?,
        tip_session_id: parse_uuid(&row.get::<_, String>(5)?)?,
        observed_event_sequence: row.get(6)?,
        observed_custody_generation: row.get(7)?,
        dedup_key: row.get(8)?,
        state: match state.as_str() {
            "intent" => RelaunchState::Intent,
            "launched" => RelaunchState::Launched,
            "abandoned" => RelaunchState::Abandoned,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        invocation_id: row
            .get::<_, Option<String>>(10)?
            .map(|value| parse_uuid(&value))
            .transpose()?,
        receipt_json: row.get(11)?,
        abandon_reason: row.get(12)?,
    })
}

const ROW_SQL: &str = "SELECT request_id,key_digest,request_fingerprint,caller_session_id,target_session_id,tip_session_id,observed_event_sequence,observed_custody_generation,dedup_key,state,invocation_id,receipt_json,abandon_reason FROM agent_child_relaunch_intents";

impl Store {
    pub(crate) fn child_relaunch_intent_by_key(
        &self,
        key_digest: &str,
    ) -> Result<Option<RelaunchIntentRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("{ROW_SQL} WHERE key_digest=?1"),
                [key_digest],
                read_row,
            )
            .optional()?)
    }

    pub(crate) fn insert_child_relaunch_intent(
        &self,
        intent: &RelaunchIntentRow,
    ) -> Result<InsertRelaunchIntent> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = tx
            .query_row(
                &format!("{ROW_SQL} WHERE key_digest=?1"),
                [&intent.key_digest],
                read_row,
            )
            .optional()?
        {
            return Ok(InsertRelaunchIntent::Existing(Box::new(existing)));
        }
        let open: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_child_relaunch_intents WHERE tip_session_id=?1 AND state='intent')",
            [intent.tip_session_id.to_string()],
            |row| row.get(0),
        )?;
        if open {
            return Ok(InsertRelaunchIntent::TipInProgress);
        }
        tx.execute(
            "INSERT INTO agent_child_relaunch_intents
             (request_id,key_digest,request_fingerprint,caller_session_id,target_session_id,tip_session_id,
              observed_event_sequence,observed_custody_generation,dedup_key,state,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'intent',?10)",
            params![intent.request_id.to_string(), intent.key_digest, intent.request_fingerprint,
                intent.caller_session_id.to_string(), intent.target_session_id.to_string(),
                intent.tip_session_id.to_string(), intent.observed_event_sequence,
                intent.observed_custody_generation, intent.dedup_key,
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)],
        )?;
        tx.commit()?;
        Ok(InsertRelaunchIntent::Inserted)
    }

    pub(crate) fn bind_child_relaunch_invocation(
        &self,
        request_id: Uuid,
        invocation_id: Uuid,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE agent_child_relaunch_intents SET invocation_id=?1 WHERE request_id=?2 AND state='intent' AND invocation_id IS NULL",
            params![invocation_id.to_string(), request_id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "child relaunch invocation binding changed".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn settle_child_relaunch_intent(
        &self,
        request_id: Uuid,
        state: RelaunchState,
        invocation_id: Option<Uuid>,
        receipt_json: &str,
        abandon_reason: Option<&str>,
    ) -> Result<()> {
        if state == RelaunchState::Intent {
            return Err(DaemonError::Store("cannot settle to intent".into()));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let changed = tx.execute(
            "UPDATE agent_child_relaunch_intents SET state=?1,invocation_id=?2,receipt_json=?3,abandon_reason=?4,settled_at=?5 WHERE request_id=?6 AND state='intent'",
            params![if state == RelaunchState::Launched { "launched" } else { "abandoned" },
                invocation_id.map(|id| id.to_string()), receipt_json, abandon_reason, now,
                request_id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "child relaunch settlement changed".into(),
            ));
        }
        if state == RelaunchState::Launched {
            tx.execute(
                "UPDATE sessions SET model_invocation_id=?1,updated_at=?2 WHERE id=(SELECT tip_session_id FROM agent_child_relaunch_intents WHERE request_id=?3)",
                params![invocation_id.map(|id| id.to_string()), now, request_id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn open_child_relaunch_intents(&self) -> Result<Vec<RelaunchIntentRow>> {
        let mut statement = self.conn.prepare(&format!(
            "{ROW_SQL} WHERE state='intent' ORDER BY created_at,request_id"
        ))?;
        Ok(statement
            .query_map([], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Classify an open request from durable evidence. This transaction never
    /// admits a model invocation or launches a provider.
    pub(crate) fn recover_child_relaunch_intent(
        &self,
        request_id: Uuid,
        task_source_session_id: Uuid,
        runtime_active: bool,
    ) -> Result<RelaunchIntentRow> {
        // The caller has reaped this tip's orphan provider when it was not
        // active. Bind an admission left between commits, complete that orphan,
        // and settle the intent under one IMMEDIATE transaction.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut row = tx.query_row(
            &format!("{ROW_SQL} WHERE request_id=?1"),
            [request_id.to_string()],
            read_row,
        )?;
        if row.state != RelaunchState::Intent {
            return Ok(row);
        }
        self.bind_and_complete_relaunch_admission_tx(&tx, &mut row, runtime_active)?;
        let (status, captured_id, bound_invocation): (String, Option<String>, Option<String>) = tx
            .query_row(
                "SELECT status,claude_session_id,model_invocation_id FROM sessions WHERE id=?1",
                [row.tip_session_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        let later_event: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1 AND sequence>?2)",
            params![row.tip_session_id.to_string(), row.observed_event_sequence],
            |r| r.get(0),
        )?;
        let active_bound = runtime_active
            && matches!(status.as_str(), "Starting" | "Running" | "WaitingApproval")
            && row
                .invocation_id
                .is_some_and(|id| bound_invocation.as_deref() == Some(id.to_string().as_str()));
        let launched =
            row.invocation_id.is_some() && (active_bound || captured_id.is_some() || later_event);
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let (state, receipt_json, abandon_reason) = if launched {
            let result = AgentContinueChildResultV1 {
                target_session_id: row.target_session_id,
                continued_session_id: row.tip_session_id,
                observed: AgentContinuationCursorV1 {
                    tip_session_id: row.tip_session_id,
                    event_sequence: row.observed_event_sequence,
                    custody_generation: row.observed_custody_generation,
                },
                watch_rearmed: false,
                relaunch: Some(AgentContinueRelaunchV1 {
                    mode: "fresh".into(),
                    reason: "no_provider_session_id".into(),
                    request_id,
                    task_source_session_id,
                    invocation_id: row
                        .invocation_id
                        .expect("launched evidence requires admission"),
                    deduplicated: false,
                    recovered: true,
                }),
            };
            ("launched", serde_json::to_string(&result)?, None)
        } else {
            ("abandoned",serde_json::json!({"request_id":request_id,"reason":"no_launch_evidence","invocation_id":row.invocation_id}).to_string(),Some("no_launch_evidence"))
        };
        tx.execute(
            "UPDATE agent_child_relaunch_intents SET state=?1,receipt_json=?2,abandon_reason=?3,settled_at=?4 WHERE request_id=?5 AND state='intent'",
            params![state,receipt_json,abandon_reason,now,request_id.to_string()],
        )?;
        if launched {
            tx.execute(
                "UPDATE sessions SET model_invocation_id=?1,updated_at=?2 WHERE id=?3",
                params![
                    row.invocation_id.map(|id| id.to_string()),
                    now,
                    row.tip_session_id.to_string()
                ],
            )?;
        }
        tx.commit()?;
        row.state = if launched {
            RelaunchState::Launched
        } else {
            RelaunchState::Abandoned
        };
        row.receipt_json = Some(receipt_json);
        row.abandon_reason = abandon_reason.map(str::to_owned);
        Ok(row)
    }

    fn bind_and_complete_relaunch_admission_tx(
        &self,
        tx: &Transaction<'_>,
        row: &mut RelaunchIntentRow,
        runtime_active: bool,
    ) -> Result<()> {
        let admitted: Option<(String, String)> = tx
            .query_row(
                "SELECT id,status FROM model_invocations WHERE dedup_key=?1 AND request_fingerprint=?2 AND session_id=?3 AND admission_status='admitted'",
                params![row.dedup_key, row.request_fingerprint, row.tip_session_id.to_string()],
                |admission| Ok((admission.get(0)?, admission.get(1)?)),
            )
            .optional()?;
        if let Some((id, status)) = admitted {
            let invocation_id = Uuid::parse_str(&id).map_err(|error| {
                DaemonError::Store(format!("invalid child relaunch invocation: {error}"))
            })?;
            if row.invocation_id.is_none() {
                let changed = tx.execute(
                    "UPDATE agent_child_relaunch_intents SET invocation_id=?1 WHERE request_id=?2 AND state='intent' AND invocation_id IS NULL",
                    params![id, row.request_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(DaemonError::Store(
                        "child relaunch invocation binding changed".into(),
                    ));
                }
                row.invocation_id = Some(invocation_id);
            } else if row.invocation_id != Some(invocation_id) {
                return Err(DaemonError::Store(
                    "child relaunch admission binding mismatch".into(),
                ));
            }
            if !runtime_active && matches!(status.as_str(), "running" | "cancellation_requested") {
                self.complete_model_invocation_in_tx(
                    tx,
                    invocation_id,
                    &crate::model_control::InvocationCompletion {
                        error_class: Some("child_relaunch_orphan_recovered".into()),
                        confidence: Some(
                            rsi_common::model_control::ModelUsageConfidence::Unavailable,
                        ),
                        ..Default::default()
                    },
                    None,
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use rusqlite::params;

    fn fixture() -> (Store, Uuid, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let caller = Uuid::new_v4();
        let tip = Uuid::new_v4();
        let cwd = std::env::temp_dir();
        store
            .insert_session(&test_session(caller, cwd.clone()))
            .unwrap();
        let mut child = test_session(tip, cwd);
        child.parent_id = Some(caller);
        child.claude_session_id = None;
        store.insert_session(&child).unwrap();
        (store, caller, tip)
    }

    fn intent(caller: Uuid, tip: Uuid, key: &str) -> RelaunchIntentRow {
        let request_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes());
        RelaunchIntentRow {
            request_id,
            key_digest: crate::model_control::hash_request_fingerprint(&[key]),
            request_fingerprint: crate::model_control::hash_request_fingerprint(&[key, "query"]),
            caller_session_id: caller,
            target_session_id: tip,
            tip_session_id: tip,
            observed_event_sequence: 0,
            observed_custody_generation: None,
            dedup_key: format!("agent.child_relaunch.v1:{request_id}"),
            state: RelaunchState::Intent,
            invocation_id: None,
            receipt_json: None,
            abandon_reason: None,
        }
    }

    fn admission(store: &Store, tip: Uuid, id: Uuid, dedup_key: &str, fingerprint: &str) {
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        store.conn.execute(
            "INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,dedup_key,request_fingerprint,policy_snapshot_json,usage_confidence,created_at) VALUES(?1,'session.launch.fresh','session_lifecycle','foreground','paid_capable','admitted','running','agent_child_relaunch',?2,?3,?4,'{}','unavailable',?5)",
            params![id.to_string(),tip.to_string(),dedup_key,fingerprint,now],
        ).unwrap();
    }

    #[test]
    fn relaunch_intent_rows_reject_delete_and_settled_update() {
        let (store, caller, tip) = fixture();
        let row = intent(caller, tip, "immutable");
        store.insert_child_relaunch_intent(&row).unwrap();
        assert!(
            store
                .conn
                .execute(
                    "DELETE FROM agent_child_relaunch_intents WHERE request_id=?1",
                    [row.request_id.to_string()]
                )
                .is_err()
        );
        let invocation = Uuid::new_v4();
        admission(
            &store,
            tip,
            invocation,
            &row.dedup_key,
            &row.request_fingerprint,
        );
        store
            .bind_child_relaunch_invocation(row.request_id, invocation)
            .unwrap();
        store
            .settle_child_relaunch_intent(
                row.request_id,
                RelaunchState::Launched,
                Some(invocation),
                "{}",
                None,
            )
            .unwrap();
        assert!(
            store
                .conn
                .execute(
                    "UPDATE agent_child_relaunch_intents SET receipt_json='{}' WHERE request_id=?1",
                    [row.request_id.to_string()]
                )
                .is_err()
        );
        assert_eq!(
            store
                .child_relaunch_intent_by_key(&row.key_digest)
                .unwrap()
                .unwrap()
                .state,
            RelaunchState::Launched
        );
    }

    #[test]
    fn fresh_relaunch_crash_after_intent_before_admission_settles_abandoned() {
        let (store, caller, tip) = fixture();
        let first = intent(caller, tip, "first");
        store.insert_child_relaunch_intent(&first).unwrap();
        let recovered = store
            .recover_child_relaunch_intent(first.request_id, tip, false)
            .unwrap();
        assert_eq!(recovered.state, RelaunchState::Abandoned);
        assert_eq!(
            recovered.abandon_reason.as_deref(),
            Some("no_launch_evidence")
        );
        let second = intent(caller, tip, "second");
        assert!(matches!(
            store.insert_child_relaunch_intent(&second).unwrap(),
            InsertRelaunchIntent::Inserted
        ));
    }

    #[test]
    fn fresh_relaunch_second_key_while_intent_open_is_in_progress() {
        let (store, caller, tip) = fixture();
        let first = intent(caller, tip, "first");
        store.insert_child_relaunch_intent(&first).unwrap();
        let second = intent(caller, tip, "second");
        assert!(matches!(
            store.insert_child_relaunch_intent(&second).unwrap(),
            InsertRelaunchIntent::TipInProgress
        ));
        assert_eq!(store.open_child_relaunch_intents().unwrap().len(), 1);
    }

    #[test]
    fn fresh_relaunch_crash_after_spawn_before_confirmation_recovers_as_launched() {
        let (store, caller, tip) = fixture();
        let row = intent(caller, tip, "spawned");
        store.insert_child_relaunch_intent(&row).unwrap();
        let invocation = Uuid::new_v4();
        admission(
            &store,
            tip,
            invocation,
            &row.dedup_key,
            &row.request_fingerprint,
        );
        store
            .bind_child_relaunch_invocation(row.request_id, invocation)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET claude_session_id='captured-provider' WHERE id=?1",
                [tip.to_string()],
            )
            .unwrap();
        let recovered = store
            .recover_child_relaunch_intent(row.request_id, tip, false)
            .unwrap();
        assert_eq!(recovered.state, RelaunchState::Launched);
        assert!(
            recovered
                .receipt_json
                .as_deref()
                .unwrap()
                .contains("\"recovered\":true")
        );
        let bound: Option<String> = store
            .conn
            .query_row(
                "SELECT model_invocation_id FROM sessions WHERE id=?1",
                [tip.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound.as_deref(), Some(invocation.to_string().as_str()));
    }

    #[test]
    fn fresh_relaunch_existing_dedup_admission_routes_to_recovery() {
        let (store, caller, tip) = fixture();
        let row = intent(caller, tip, "admitted-before-binding");
        store.insert_child_relaunch_intent(&row).unwrap();
        let invocation = Uuid::new_v4();
        admission(
            &store,
            tip,
            invocation,
            &row.dedup_key,
            &row.request_fingerprint,
        );
        let recovered = store
            .recover_child_relaunch_intent(row.request_id, tip, false)
            .unwrap();
        assert_eq!(recovered.state, RelaunchState::Abandoned);
        assert_eq!(recovered.invocation_id, Some(invocation));
        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM model_invocations WHERE id=?1",
                [invocation.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");
    }

    #[test]
    fn fresh_relaunch_recovery_settlement_fault_rolls_back_binding_and_completion() {
        let (store, caller, tip) = fixture();
        let row = intent(caller, tip, "atomic-recovery");
        store.insert_child_relaunch_intent(&row).unwrap();
        let invocation = Uuid::new_v4();
        admission(
            &store,
            tip,
            invocation,
            &row.dedup_key,
            &row.request_fingerprint,
        );
        store
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_relaunch_settlement
                 BEFORE UPDATE OF state ON agent_child_relaunch_intents
                 WHEN NEW.state!='intent'
                 BEGIN SELECT RAISE(ABORT,'injected_settlement_fault'); END;",
            )
            .unwrap();
        assert!(
            store
                .recover_child_relaunch_intent(row.request_id, tip, false)
                .is_err()
        );
        let intent = store
            .child_relaunch_intent_by_key(&row.key_digest)
            .unwrap()
            .unwrap();
        assert_eq!(intent.state, RelaunchState::Intent);
        assert_eq!(intent.invocation_id, None);
        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM model_invocations WHERE id=?1",
                [invocation.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");
        store
            .conn
            .execute_batch("DROP TRIGGER fail_relaunch_settlement")
            .unwrap();
        assert_eq!(
            store
                .recover_child_relaunch_intent(row.request_id, tip, false)
                .unwrap()
                .state,
            RelaunchState::Abandoned
        );
    }

    #[test]
    fn fresh_relaunch_two_keys_each_replay_returns_its_own_receipt() {
        let (store, caller, tip) = fixture();
        let mut ids = Vec::new();
        for key in ["A", "B"] {
            let row = intent(caller, tip, key);
            store.insert_child_relaunch_intent(&row).unwrap();
            let invocation = Uuid::new_v4();
            admission(
                &store,
                tip,
                invocation,
                &row.dedup_key,
                &row.request_fingerprint,
            );
            store
                .bind_child_relaunch_invocation(row.request_id, invocation)
                .unwrap();
            let receipt = format!(
                "{{\"request_id\":\"{}\",\"invocation_id\":\"{}\"}}",
                row.request_id, invocation
            );
            store
                .settle_child_relaunch_intent(
                    row.request_id,
                    RelaunchState::Launched,
                    Some(invocation),
                    &receipt,
                    None,
                )
                .unwrap();
            ids.push((row, invocation, receipt));
        }
        for (row, invocation, receipt) in &ids {
            let replay = store
                .child_relaunch_intent_by_key(&row.key_digest)
                .unwrap()
                .unwrap();
            assert_eq!(replay.request_id, row.request_id);
            assert_eq!(replay.invocation_id, Some(*invocation));
            assert_eq!(replay.receipt_json.as_deref(), Some(receipt.as_str()));
        }
        assert_ne!(ids[0].2, ids[1].2);
    }
}
