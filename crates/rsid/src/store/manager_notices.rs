//! Coalesced semantic notices and attributed retrieval of operator answers.
use super::Store;
use super::harness_manager_v2::{fingerprint, now, refused};
use super::manager_actions::{ManagerActionOperationV2, action_epic};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use rsi_common::{
    harness_manager::{HarnessManagerConfigV1, HarnessManagerNoticeV1},
    harness_manager_v2::*,
    types::{PendingQuestion, Session, SessionKind, SessionStatus},
};
use rusqlite::{
    OptionalExtension, Transaction, TransactionBehavior, params, params_from_iter,
    types::Value as SqlValue,
};
use serde_json::{Value, json};
use uuid::Uuid;

const MAX_RENDERED_NOTICES: i64 = 16;
const MAX_NOTICE_RECONCILIATION_BATCH: i64 = 256;
const MAX_NOTICE_RETRIEVAL_SCAN: i64 = 256;
const MAX_OBSOLETE_NOTICES_PER_JOB: i64 = 16;
const MAX_SCOPE_RETIREMENTS_PER_PASS: i64 = 8;
const MAX_NOTICE_QUESTION_BYTES: usize = 8192;
const MAX_NOTICE_RECORDS_PER_EPIC_PASS: i64 = 16;

// Keep the executed statement shared with its physical-work regression. The
// route equalities precede sequence in V118, so LIMIT bounds the scanned page
// as well as the mutation count even with unrelated retained pending notices.
const RETIRE_OBSOLETE_MANAGER_NOTICE_ROUTE_SQL: &str =
    "UPDATE harness_manager_notices SET retired_at=?2
     WHERE id IN (
         SELECT id FROM harness_manager_notices
              INDEXED BY harness_manager_notices_pending_job_route_sequence
         WHERE job_id=?1 AND project_id=?3 AND manager_session_id=?4
           AND scope_version=?5 AND direction='to_manager'
           AND retired_at IS NULL AND settled_at IS NULL
         ORDER BY sequence LIMIT ?6
     )";

/// What [`Store::abandon_unconsumed_watch`] newly recorded.
#[derive(Debug, Default)]
pub struct DeliveryAbandonedRecord {
    /// The persisted health-fact event, when this call recorded it.
    pub health_event: Option<rsi_common::types::ConversationEvent>,
    /// Manager-notice transport to publish after commit, when queued.
    pub notice_job: Option<Uuid>,
}

struct BoundedQuestionWriter {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl BoundedQuestionWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(MAX_NOTICE_QUESTION_BYTES),
            overflowed: false,
        }
    }
}

impl std::io::Write for BoundedQuestionWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let remaining = MAX_NOTICE_QUESTION_BYTES.saturating_sub(self.bytes.len());
        if buf.len() > remaining {
            self.overflowed = true;
            return Err(std::io::Error::other(
                "manager notice question exceeds projection limit",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn manager_notice_question_projection(question: &Option<PendingQuestion>) -> Result<Value> {
    let mut writer = BoundedQuestionWriter::new();
    match serde_json::to_writer(&mut writer, question) {
        Ok(()) => return Ok(serde_json::from_slice(&writer.bytes)?),
        Err(error) if !writer.overflowed => return Err(error.into()),
        Err(_) => {}
    }
    Ok(json!({
        "state": "requires_session_view",
        "safe_error_class": "manager_question_requires_session_view",
        "question_count": question.as_ref().map_or(0, |pending| pending.questions.len()),
        "serialized_bytes_at_least": MAX_NOTICE_QUESTION_BYTES + 1,
        "projection_limit_bytes": MAX_NOTICE_QUESTION_BYTES,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagerActionNoticeFault {
    AfterWatch,
    AfterNotice,
}

#[cfg(test)]
thread_local! {
    static MANAGER_ACTION_NOTICE_FAIL_NEXT: std::cell::Cell<Option<ManagerActionNoticeFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn manager_action_notice_fail_next(point: ManagerActionNoticeFault) {
    MANAGER_ACTION_NOTICE_FAIL_NEXT.with(|slot| slot.set(Some(point)));
}

fn manager_action_notice_fault(point: ManagerActionNoticeFault) -> Result<()> {
    #[cfg(test)]
    if MANAGER_ACTION_NOTICE_FAIL_NEXT
        .with(|slot| slot.get() == Some(point) && slot.take().is_some())
    {
        return Err(DaemonError::Store(format!(
            "injected manager action notice fault: {point:?}"
        )));
    }
    let _ = point;
    Ok(())
}

/// Test-only failure points inside the issue #648 atomic abandonment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryAbandonedFault {
    AfterRetirement,
    AfterHealthFact,
}

#[cfg(test)]
thread_local! {
    static DELIVERY_ABANDONED_FAIL_NEXT: std::cell::Cell<Option<DeliveryAbandonedFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
impl Store {
    /// Fail this thread's next issue #648 abandonment after the watch row is
    /// retired, before the health fact (the whole transaction rolls back).
    pub(crate) fn fail_next_delivery_abandonment_after_retirement() {
        DELIVERY_ABANDONED_FAIL_NEXT
            .with(|slot| slot.set(Some(DeliveryAbandonedFault::AfterRetirement)));
    }

    /// Fail this thread's next issue #648 abandonment after the health fact,
    /// before the manager notice (the whole transaction rolls back).
    pub(crate) fn fail_next_delivery_abandonment_after_health_fact() {
        DELIVERY_ABANDONED_FAIL_NEXT
            .with(|slot| slot.set(Some(DeliveryAbandonedFault::AfterHealthFact)));
    }
}

// `Result` and non-const are needed by the test-only failure branch.
#[allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)]
fn delivery_abandoned_fault(point: DeliveryAbandonedFault) -> Result<()> {
    #[cfg(test)]
    if DELIVERY_ABANDONED_FAIL_NEXT.with(|slot| slot.get() == Some(point) && slot.take().is_some())
    {
        return Err(DaemonError::Store(format!(
            "injected delivery abandonment fault: {point:?}"
        )));
    }
    let _ = point;
    Ok(())
}

#[derive(Debug)]
struct StoredNotice {
    public: HarnessManagerNoticeV1,
    job_id: Uuid,
}

const NOTICE_COLUMNS: &str = "id,sequence,epic_id,direction,kind,subject_id,subject_version,
     source_session_id,recipient_session_id,state_json,recorded_at,queued_at,
     delivered_at,retrieved_at,settled_at,job_id";

fn read_notice(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredNotice> {
    fn id(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Uuid> {
        let text: String = row.get(column)?;
        Uuid::parse_str(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }
    fn at(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<DateTime<Utc>> {
        let text: String = row.get(column)?;
        super::parse_timestamp(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error)),
            )
        })
    }
    fn maybe_at(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
        row.get::<_, Option<String>>(column)?
            .map(|text| {
                super::parse_timestamp(&text).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        column,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(error)),
                    )
                })
            })
            .transpose()
    }
    let source: Option<String> = row.get(7)?;
    let state: String = row.get(9)?;
    Ok(StoredNotice {
        public: HarnessManagerNoticeV1 {
            notice_id: id(row, 0)?,
            sequence: row.get(1)?,
            epic_id: row
                .get::<_, Option<String>>(2)?
                .map(|value| Uuid::parse_str(&value))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            direction: row.get(3)?,
            kind: row.get(4)?,
            subject_id: row.get(5)?,
            subject_version: row.get(6)?,
            source_session_id: source
                .map(|value| Uuid::parse_str(&value))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            recipient_session_id: id(row, 8)?,
            state: serde_json::from_str(&state).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
            recorded_at: at(row, 10)?,
            queued_at: at(row, 11)?,
            delivered_at: maybe_at(row, 12)?,
            retrieved_at: maybe_at(row, 13)?.unwrap_or_else(Utc::now),
            settled_at: maybe_at(row, 14)?.unwrap_or_else(Utc::now),
        },
        job_id: id(row, 15)?,
    })
}

impl Store {
    pub(crate) fn manager_notice_job_for_subject(
        &self,
        kind: &str,
        subject_id: &str,
        subject_version: &str,
    ) -> Result<Option<Uuid>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT job_id FROM harness_manager_notices
             WHERE kind=?1 AND subject_id=?2 AND subject_version=?3
               AND retired_at IS NULL AND settled_at IS NULL AND delivered_at IS NULL
             ORDER BY sequence DESC LIMIT 1",
                params![kind, subject_id, subject_version],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|value| {
            Uuid::parse_str(&value).map_err(|_| refused("manager_invalid_stored_identity"))
        })
        .transpose()
    }

    fn insert_manager_notice(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        to_manager: bool,
        kind: &str,
        subject_id: &str,
        subject_version: &str,
        source_session_id: Option<Uuid>,
        state: &Value,
    ) -> Result<Option<Uuid>> {
        let lead = self.manager_lead(config.project_id, epic)?;
        let (job_id, route_source, recipient, _) =
            self.manager_watch_identity(config, &lead, to_manager)?;
        if route_source == recipient {
            return Ok(None);
        }
        let existing_unsettled: Option<bool> = self
            .conn
            .query_row(
                "SELECT retired_at IS NULL AND settled_at IS NULL FROM harness_manager_notices
             WHERE job_id=?1 AND kind=?2 AND subject_id=?3 AND subject_version=?4",
                params![job_id.to_string(), kind, subject_id, subject_version],
                |row| row.get(0),
            )
            .optional()?;
        if existing_unsettled.is_some() {
            // Reconciliation is also restart recovery. Restore a disabled
            // transport from retained notice state without manufacturing a
            // new notice. Another subject on this coalesced job may still be
            // pending even when this exact subject was already retrieved. Do
            // not refresh an already-enabled row: the scheduler captures its
            // due instant before reconciliation, and moving that instant
            // forward on every pass would starve delivery forever.
            if self.manager_watch_delivery_state(job_id)?.is_some()
                && self
                    .get_scheduled_job(&job_id)?
                    .is_none_or(|job| !job.enabled)
            {
                self.ensure_manager_watch(config, &lead, to_manager, subject_version, false)?;
                self.refresh_manager_notice_job(job_id)?;
            }
            return Ok(None);
        }
        // Automatic manager-facing notices remain durable even when all 64
        // transport slots are occupied. The deterministic job identity can be
        // recorded before its scheduled row exists; reconciliation creates the
        // transport when a slot becomes available. Lead-facing mail retains
        // ensure_manager_watch's explicit capacity refusal and transaction
        // rollback.
        self.ensure_manager_watch(config, &lead, to_manager, subject_version, false)?;
        let direction = if to_manager { "to_manager" } else { "to_lead" };
        let recorded_at = now();
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14)",
            params![
                Uuid::new_v4().to_string(),
                job_id.to_string(),
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                epic.to_string(),
                direction,
                source_session_id.map(|id| id.to_string()),
                recipient.to_string(),
                kind,
                subject_id,
                subject_version,
                serde_json::to_string(state)?,
                recorded_at
            ],
        )?;
        self.refresh_manager_notice_job(job_id)?;
        Ok(Some(job_id))
    }

    pub(crate) fn record_manager_session_notice(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &Session,
    ) -> Result<Option<Uuid>> {
        let epic = lead
            .parent_id
            .ok_or_else(|| refused("manager_legal_epic_required"))?;
        let event_sequence: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
            [lead.id.to_string()],
            |row| row.get(0),
        )?;
        let model_invocation_id: Option<String> = self.conn.query_row(
            "SELECT model_invocation_id FROM sessions WHERE id=?1",
            [lead.id.to_string()],
            |row| row.get(0),
        )?;
        let question = manager_notice_question_projection(&lead.pending_question)?;
        let state = json!({
            "session_id":lead.id,
            "status":lead.status,
            "stop_reason":lead.stop_reason,
            "question":question,
            "model_invocation_id":model_invocation_id,
            "event_sequence":event_sequence,
            "session_updated_at":lead.updated_at,
        });
        let version = fingerprint(&state)?;
        self.insert_manager_notice(
            config,
            epic,
            true,
            "session_state",
            &lead.id.to_string(),
            &version,
            Some(lead.id),
            &state,
        )
    }

    /// Capture the final exact session state while the Epic still names this
    /// lead. Auto-archive may remove that live pointer immediately afterwards;
    /// the retained notice and its deterministic route remain the retry owner.
    pub(crate) fn record_manager_terminal_notice_before_archive(
        &self,
        lead: &Session,
    ) -> Result<Option<Uuid>> {
        if !matches!(
            lead.status,
            SessionStatus::Completed
                | SessionStatus::Failed
                | SessionStatus::Interrupted
                | SessionStatus::WaitingApproval
        ) {
            return Ok(None);
        }
        let (Some(project), Some(epic)) = (lead.project_id, lead.parent_id) else {
            return Ok(None);
        };
        let Some(config) = self.get_harness_manager_notice_config(project)? else {
            return Ok(None);
        };
        if config.current_session_id.is_none() || !self.manager_config_covers_epic(&config, epic)? {
            return Ok(None);
        }
        let Ok(current_lead) = self.manager_lead(project, epic) else {
            return Ok(None);
        };
        if current_lead.id != lead.id {
            return Ok(None);
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let job = self.record_manager_session_notice(&config, lead)?;
        transaction.commit()?;
        Ok(job)
    }

    pub(crate) fn record_manager_message_notice(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        receipt: &rsi_common::harness_manager::HarnessManagerMessageReceiptV1,
        to_manager: bool,
    ) -> Result<Option<Uuid>> {
        let epic = lead
            .parent_id
            .ok_or_else(|| refused("manager_legal_epic_required"))?;
        self.insert_manager_notice(
            config,
            epic,
            to_manager,
            "message",
            &receipt.message_id.to_string(),
            &receipt.sequence.to_string(),
            Some(if to_manager {
                lead.id
            } else {
                config.manager_session_id
            }),
            &json!({"message_id":receipt.message_id,"message_sequence":receipt.sequence,
                    "request_id":receipt.request_id}),
        )
    }

    fn manager_action_notice_epic(
        &self,
        operation: &ManagerActionOperationV2,
    ) -> Result<Option<Uuid>> {
        use ManagerActionV2::*;
        if let Some(epic) = action_epic(&operation.context.request.operation) {
            return Ok(Some(epic));
        }
        match &operation.context.request.operation {
            CreateContainer { kind, .. } if *kind == SessionKind::Epic => {
                Ok(operation.context.target_session_id)
            }
            CreateSession { parent_id, .. } => Ok(Some(*parent_id)),
            UpdateContainer { .. }
            | ArchiveContainer { .. }
            | DeleteContainer { .. }
            | RestoreContainer { .. } => {
                let Some(target) = operation.context.target_session_id else {
                    return Ok(None);
                };
                Ok(self
                    .get_session(target)?
                    .filter(|session| session.session_kind == SessionKind::Epic)
                    .map(|session| session.id))
            }
            ArchiveSession { session_id, .. }
            | RestoreSession { session_id, .. }
            | UpdateSession { session_id, .. } => self.manager_action_session_epic(*session_id),
            OperatorCall { call, .. } => call
                .typed()
                .ok()
                .and_then(|c| c.session_id())
                .map_or(Ok(None), |session_id| {
                    self.manager_action_session_epic(session_id)
                }),
            SucceedManager { .. }
            | CreateContainer { .. }
            | Integrate { .. }
            | SettleUncertainAction { .. } => Ok(None),
            ResumeLead { .. }
            | PauseLead { .. }
            | RetryLead { .. }
            | ReplaceLead { .. }
            | AssignLead { .. }
            | RetireLeadContinuations { .. } => unreachable!("lead actions returned above"),
        }
    }

    fn mark_manager_action_notice_reconciled(
        &self,
        receipt: &ManagerActionReceiptV2,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE harness_manager_action_notice_queue SET reconciled_at=?3
             WHERE operation_id=?1 AND operation_row_version=?2
               AND reconciled_at IS NULL AND retired_at IS NULL",
            params![receipt.operation_id.to_string(), receipt.row_version, now()],
        )?;
        Ok(())
    }

    fn retire_unmaterializable_manager_action_notice(
        &self,
        operation_id: Uuid,
        operation_row_version: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE harness_manager_action_notice_queue SET retired_at=?3
             WHERE operation_id=?1 AND operation_row_version=?2
               AND reconciled_at IS NULL AND retired_at IS NULL",
            params![operation_id.to_string(), operation_row_version, now()],
        )?;
        Ok(())
    }

    fn record_manager_action_notice_in_transaction(
        &self,
        config: &HarnessManagerConfigV1,
        operation: &ManagerActionOperationV2,
    ) -> Result<Option<Uuid>> {
        let receipt = &operation.receipt;
        let version = receipt.row_version.to_string();
        let (job_id, _, _) = self.manager_action_watch_identity(config);
        let existing: Option<(bool, bool)> = self
            .conn
            .query_row(
                "SELECT retired_at IS NULL,settled_at IS NULL
                 FROM harness_manager_notices
                 WHERE job_id=?1 AND kind='action_result'
                   AND subject_id=?2 AND subject_version=?3",
                params![
                    job_id.to_string(),
                    receipt.operation_id.to_string(),
                    version
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((live, unsettled)) = existing {
            if live && unsettled {
                self.ensure_manager_action_watch(config, &version)?;
                manager_action_notice_fault(ManagerActionNoticeFault::AfterWatch)?;
                self.refresh_manager_notice_job(job_id)?;
            }
            self.mark_manager_action_notice_reconciled(receipt)?;
            return Ok(None);
        }
        self.ensure_manager_action_watch(config, &version)?;
        manager_action_notice_fault(ManagerActionNoticeFault::AfterWatch)?;
        let recorded_at = now();
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,?6,'to_manager',?7,?4,'action_result',?8,?9,?10,?11,?11)",
            params![
                Uuid::new_v4().to_string(),
                job_id.to_string(),
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                self.manager_action_notice_epic(operation)?
                    .map(|epic| epic.to_string()),
                operation.context.target_session_id.map(|id| id.to_string()),
                receipt.operation_id.to_string(),
                version,
                serde_json::to_string(receipt)?,
                recorded_at
            ],
        )?;
        manager_action_notice_fault(ManagerActionNoticeFault::AfterNotice)?;
        self.refresh_manager_notice_job(job_id)?;
        self.mark_manager_action_notice_reconciled(receipt)?;
        Ok(Some(job_id))
    }

    pub(crate) fn record_manager_action_notice(
        &self,
        config: &HarnessManagerConfigV1,
        operation: &ManagerActionOperationV2,
    ) -> Result<Option<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let inserted = self.record_manager_action_notice_in_transaction(config, operation)?;
        tx.commit()?;
        Ok(inserted)
    }

    fn record_manager_ledger_notice(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        record: &super::harness_manager_v2::ManagerRecordV2,
    ) -> Result<Option<Uuid>> {
        let epic = lead
            .parent_id
            .ok_or_else(|| refused("manager_legal_epic_required"))?;
        self.insert_manager_notice(
            config,
            epic,
            true,
            "ledger_change",
            &format!("{}:{}", record.kind, record.key),
            &record.row_version.to_string(),
            Some(lead.id),
            &json!({"record_kind":record.kind,"record_key":record.key,
                    "row_version":record.row_version,"payload":record.payload}),
        )
    }

    fn reconcile_manager_action_notices(&self, config: &HarnessManagerConfigV1) -> Result<()> {
        let mut statement = self.conn.prepare(
            "SELECT operation_id,operation_row_version,receipt_json
             FROM harness_manager_action_notice_queue
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND reconciled_at IS NULL AND retired_at IS NULL
             ORDER BY queued_at,operation_id,operation_row_version LIMIT ?4",
        )?;
        let obligations = statement
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    MAX_NOTICE_RECONCILIATION_BATCH
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (id, queued_version, receipt_json) in obligations {
            let id =
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let Some(mut operation) = self.manager_action_operation(id)? else {
                self.retire_unmaterializable_manager_action_notice(id, queued_version)?;
                continue;
            };
            let receipt: ManagerActionReceiptV2 = serde_json::from_str(&receipt_json)?;
            if receipt.operation_id != id
                || receipt.row_version != queued_version
                || matches!(
                    receipt.state,
                    ManagerActionStateV2::Queued | ManagerActionStateV2::Running
                )
            {
                return Err(refused("manager_v2_action_notice_queue_invalid"));
            }
            operation.receipt = receipt;
            self.record_manager_action_notice_in_transaction(config, &operation)?;
        }
        Ok(())
    }

    pub(crate) fn reconcile_manager_action_notice(
        &self,
        operation_id: Uuid,
    ) -> Result<Option<Uuid>> {
        let Some(operation) = self.manager_action_operation(operation_id)? else {
            return Ok(None);
        };
        if matches!(
            operation.receipt.state,
            ManagerActionStateV2::Queued | ManagerActionStateV2::Running
        ) {
            return Ok(None);
        }
        let Some(config) = self.get_harness_manager(operation.project_id)? else {
            return Ok(None);
        };
        if config.manager_session_id != operation.manager_session_id
            || config.row_version != operation.scope_version
        {
            return Ok(None);
        }
        let inserted = self.record_manager_action_notice(&config, &operation)?;
        if inserted.is_some() {
            return Ok(inserted);
        }
        self.manager_notice_job_for_subject(
            "action_result",
            &operation_id.to_string(),
            &operation.receipt.row_version.to_string(),
        )
    }

    pub(crate) fn record_manager_operator_answer_notice(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        decision_key: &str,
        row_version: i64,
        target_digest: &str,
    ) -> Result<Option<Uuid>> {
        let epic = lead
            .parent_id
            .ok_or_else(|| refused("manager_legal_epic_required"))?;
        self.insert_manager_notice(
            config,
            epic,
            false,
            "operator_answer",
            decision_key,
            &format!("{row_version}:{target_digest}"),
            Some(config.manager_session_id),
            &json!({"decision_key":decision_key,"row_version":row_version,
                    "target_digest":target_digest,"state":"answered"}),
        )
    }

    pub(crate) fn refresh_manager_notice_job(&self, job_id: Uuid) -> Result<()> {
        let unsettled: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE job_id=?1 AND retired_at IS NULL AND settled_at IS NULL)",
            [job_id.to_string()],
            |row| row.get(0),
        )?;
        let mut statement = self.conn.prepare(
            "SELECT kind,subject_id,subject_version
             FROM harness_manager_notices
             WHERE job_id=?1 AND retired_at IS NULL
               AND settled_at IS NULL AND delivered_at IS NULL
             ORDER BY sequence LIMIT ?2",
        )?;
        let rows = statement
            .query_map(
                params![job_id.to_string(), MAX_RENDERED_NOTICES + 1],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        if rows.is_empty() {
            if !unsettled {
                self.conn.execute(
                    "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1",
                    params![job_id.to_string(), now()],
                )?;
            }
            return Ok(());
        }
        let more = rows.len() > MAX_RENDERED_NOTICES as usize;
        let visible = rows.iter().take(MAX_RENDERED_NOTICES as usize);
        let mut lines = vec![
            "Harness manager notice: retrieve AgentManagerInbox to settle these exact durable changes. Retrieval does not answer decisions, clear questions, reply to mail, or grant approval.".to_string(),
        ];
        for (kind, subject, version) in visible {
            lines.push(format!("- {kind} subject={subject} version={version}"));
        }
        if more {
            lines.push("- additional notices remain; retrieve AgentManagerInbox again".into());
        }
        let signature = fingerprint(&json!(rows))?;
        self.conn.execute(
            "UPDATE harness_manager_watches SET attention_signature=?2 WHERE job_id=?1",
            params![job_id.to_string(), signature],
        )?;
        let job = self.get_scheduled_job(&job_id)?;
        let can_enable = if let Some(job) = &job {
            if job.enabled {
                true
            } else {
                let enabled: i64 = self.conn.query_row(
                    "SELECT count(*) FROM scheduled_jobs
                     WHERE enabled=1 AND wake_session_id=?1
                       AND wake_mode LIKE 'on_terminal:%'",
                    [job.wake_session_id.map(|id| id.to_string())],
                    |row| row.get(0),
                )?;
                enabled < crate::session::agent_verbs::MAX_TERMINAL_WATCHES_PER_MASTER as i64
            }
        } else {
            false
        };
        let rendered = lines.join("\n");
        if can_enable {
            self.conn.execute(
                "UPDATE scheduled_jobs SET message=?2,enabled=1,last_fired_at=NULL,
                 next_fire_at=?3,updated_at=?3 WHERE id=?1",
                params![job_id.to_string(), rendered, now()],
            )?;
        } else if job.is_some() {
            // Keep exact pending subjects rendered on their bound transport,
            // but do not violate the per-recipient enabled-watch cap. A later
            // reconciliation retries this disabled row after capacity frees.
            self.conn.execute(
                "UPDATE scheduled_jobs SET message=?2,updated_at=?3 WHERE id=?1",
                params![job_id.to_string(), rendered, now()],
            )?;
        }
        Ok(())
    }

    pub(crate) fn manager_watch_delivery_state(&self, job_id: Uuid) -> Result<Option<bool>> {
        let (pending, undelivered): (bool, bool) = self.conn.query_row(
            "SELECT
               EXISTS(SELECT 1 FROM harness_manager_notices
                       WHERE job_id=?1 AND retired_at IS NULL AND settled_at IS NULL),
               EXISTS(SELECT 1 FROM harness_manager_notices
                       WHERE job_id=?1 AND retired_at IS NULL
                         AND settled_at IS NULL AND delivered_at IS NULL)",
            [job_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(pending.then_some(undelivered))
    }

    /// Undelivered subject count when this durable manager transport owes its
    /// recipient a FIRST wake: at least one live subject was never delivered
    /// and no delivered subject still awaits `AgentManagerInbox` retrieval.
    /// A recipient already woken for this route reads every later subject on
    /// the same inbox pass, so a new subject coalesces instead of re-waking it.
    pub(crate) fn manager_notice_first_wake_pending(&self, job_id: Uuid) -> Result<Option<i64>> {
        let awaiting_retrieval: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
                     WHERE job_id=?1 AND retired_at IS NULL
                       AND settled_at IS NULL AND delivered_at IS NOT NULL)",
            [job_id.to_string()],
            |row| row.get(0),
        )?;
        if awaiting_retrieval {
            return Ok(None);
        }
        let undelivered = self.manager_notice_undelivered_count(job_id)?;
        Ok((undelivered > 0).then_some(undelivered))
    }

    /// Live, never-delivered subjects on one durable manager transport.
    pub(crate) fn manager_notice_undelivered_count(&self, job_id: Uuid) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM harness_manager_notices
             WHERE job_id=?1 AND retired_at IS NULL
               AND settled_at IS NULL AND delivered_at IS NULL",
            [job_id.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Durable manager-facing notice for a review assignment that just became
    /// terminal (`submitted` or `failed`). Runs inside the caller's transition
    /// transaction. Review terminal states ride the existing `ledger_change`
    /// kind with subject `review:<assignment_id>`; the subject version is the
    /// terminal state, so each assignment yields at most one notice per state.
    ///
    /// The notice is best-effort relative to the review transition: when the
    /// project has no live manager, the Epic is out of scope, or the Epic has
    /// no current lead to anchor the route, no notice is recorded and the
    /// transition still commits.
    pub(crate) fn record_manager_review_notice(
        &self,
        project_id: Uuid,
        epic_id: Uuid,
        assignment_id: Uuid,
        terminal_state: &str,
        state: &Value,
    ) -> Result<Option<Uuid>> {
        let Some(config) = self.get_harness_manager_notice_config(project_id)? else {
            return Ok(None);
        };
        if config.current_session_id.is_none()
            || !self.manager_config_covers_epic(&config, epic_id)?
        {
            return Ok(None);
        }
        let Ok(lead) = self.manager_lead(project_id, epic_id) else {
            return Ok(None);
        };
        self.insert_manager_notice(
            &config,
            epic_id,
            true,
            "ledger_change",
            &format!("review:{assignment_id}"),
            terminal_state,
            Some(lead.id),
            state,
        )
    }

    /// Issue #648: atomically abandon a terminal watch whose delivery the wake
    /// tip never consumed.
    ///
    /// ONE IMMEDIATE transaction retires the watch row, records the tip's
    /// health fact and, when applicable, queues the manager notice. A crash or
    /// `SQLite` failure anywhere before commit leaves the watch armed and
    /// unchanged, so the next scheduler pass re-plans the same give-up and
    /// retries the whole settlement. Nothing is ever retired without its fact.
    ///
    /// 1. Retirement: a child watch is retired only from the planned row
    ///    version (`retire_unchanged_child_watch_in`); a manager watch only
    ///    from the captured notice generation (`update_watch_fire_row`). A
    ///    stale snapshot of either returns `Ok(None)` and records nothing.
    /// 2. Health fact: ONE `System` conversation event on the tip whose
    ///    metadata carries `health_fact = "delivery_abandoned"` and the job
    ///    id. It is the tip's durable, transcript-visible "cannot be resumed"
    ///    marker; `SessionStatus` is deliberately unchanged. An existing fact
    ///    for (tip, job) is not duplicated.
    /// 3. Manager notice: only when the tip is the current lead of an Epic in
    ///    a live harness-manager scope, one `to_manager` `session_state`
    ///    notice with subject version `delivery_abandoned:<job_id>`.
    ///    `insert_manager_notice` dedupes on the exact subject version, so a
    ///    (lead, job) pair notifies at most once.
    ///
    /// `sequence_floor` keeps the event ahead of the caller's in-memory
    /// transcript cache. The caller publishes only after this returns.
    pub(crate) fn abandon_unconsumed_watch(
        &self,
        job: &rsi_common::types::ScheduledJob,
        capture: &super::manager_watch_settlement::WatchFireCapture,
        delivery: &crate::issue_tracker::poller::UnconsumedDelivery,
        abandoned_at: DateTime<Utc>,
        sequence_floor: i32,
    ) -> Result<Option<DeliveryAbandonedRecord>> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if capture.is_manager_watch(job.id) {
            // Generation-fenced disable. A rearm, disable or any other write
            // after capture advances notice_generation, so 0 rows means this
            // capture is stale: return before writing anything. Dropping the
            // uncommitted transaction leaves the concurrent writer's row, its
            // generation, and every fact/notice exactly as they were.
            if self.update_watch_fire_row(
                capture.manager_generation(job.id),
                job.id,
                Some(&abandoned_at),
                None,
                false,
                // Terminal disable: reset the K2 retry state in the same
                // UPDATE, matching the child-watch branch below.
                true,
            )? != 1
            {
                return Ok(None);
            }
        } else if !Self::retire_unchanged_child_watch_in(&transaction, job, "abandoned")? {
            return Ok(None);
        }
        delivery_abandoned_fault(DeliveryAbandonedFault::AfterRetirement)?;

        let mut record = DeliveryAbandonedRecord::default();
        let Some(tip) = self.get_session(delivery.tip)? else {
            transaction.commit()?;
            return Ok(Some(record));
        };
        let job_id = job.id;
        let abandoned_at_text = abandoned_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let recorded: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM conversation_events
                WHERE session_id=?1 AND event_type='System'
                  AND json_extract(metadata,'$.health_fact')='delivery_abandoned'
                  AND json_extract(metadata,'$.job_id')=?2
             )",
            params![tip.id.to_string(), job_id.to_string()],
            |row| row.get(0),
        )?;
        if !recorded {
            let next_sequence: i64 = transaction.query_row(
                "SELECT COALESCE(MAX(sequence)+1,0) FROM conversation_events
                 WHERE session_id=?1",
                [tip.id.to_string()],
                |row| row.get(0),
            )?;
            let sequence = i32::try_from(next_sequence)
                .map_err(|_| DaemonError::Store("conversation sequence overflow".into()))?
                .max(sequence_floor);
            let mut event = rsi_common::types::ConversationEvent {
                id: 0,
                session_id: tip.id,
                sequence,
                event_type: rsi_common::types::EventType::System,
                role: None,
                content: format!(
                    "[rsid-health] delivery_abandoned: terminal-watch job {job_id} was never \
                     consumed by this session (no provider output {} min after watched \
                     session {} ended). The watch is retired; this session needs a manual \
                     resume or replacement.",
                    delivery.minutes, delivery.watched
                ),
                tool_name: None,
                tool_input: None,
                created_at: abandoned_at,
                offload_id: None,
                tool_use_id: None,
                metadata: Some(Box::new(json!({
                    "health_fact": "delivery_abandoned",
                    "job_id": job_id,
                    "watched_session_id": delivery.watched,
                    "minutes_unconsumed": delivery.minutes,
                    "abandoned_at": abandoned_at_text,
                }))),
            };
            event.id = Self::insert_event_in_transaction(&transaction, &event, None)?;
            record.health_event = Some(event);
        }
        delivery_abandoned_fault(DeliveryAbandonedFault::AfterHealthFact)?;

        if let Some((config, epic)) = self.delivery_abandoned_notice_route(&tip)? {
            record.notice_job = self.insert_manager_notice(
                &config,
                epic,
                true,
                "session_state",
                &tip.id.to_string(),
                &format!("delivery_abandoned:{job_id}"),
                Some(tip.id),
                &json!({
                    "lead_session_id": tip.id,
                    "epic_id": epic,
                    "watched_session_id": delivery.watched,
                    "job_id": job_id,
                    "minutes_unconsumed": delivery.minutes,
                    "abandoned_at": abandoned_at_text,
                }),
            )?;
        }
        transaction.commit()?;
        Ok(Some(record))
    }

    /// The live manager scope and Epic when `tip` is that Epic's current lead.
    fn delivery_abandoned_notice_route(
        &self,
        tip: &Session,
    ) -> Result<Option<(HarnessManagerConfigV1, Uuid)>> {
        let (Some(project), Some(epic)) = (tip.project_id, tip.parent_id) else {
            return Ok(None);
        };
        let Some(config) = self.get_harness_manager_notice_config(project)? else {
            return Ok(None);
        };
        if config.current_session_id.is_none() || !self.manager_config_covers_epic(&config, epic)? {
            return Ok(None);
        }
        let Ok(lead) = self.manager_lead(project, epic) else {
            return Ok(None);
        };
        Ok((lead.id == tip.id).then_some((config, epic)))
    }

    /// Admit manager-facing transports that were deferred by the per-recipient
    /// enabled-watch cap. Durable notice rows are the retry owner: scheduler
    /// reconciliation and inbox settlement can recover an exact job even when
    /// the lead is still running and no new session-state event is produced.
    pub(crate) fn reconcile_deferred_manager_notice_transports(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<()> {
        let after = self.manager_notice_cursor_after(config, "deferred_transports", None)?;
        let (after_sequence, after_job) = if after.is_empty() {
            (0, String::new())
        } else {
            let (sequence, job) = after
                .split_once(':')
                .ok_or_else(|| refused("manager_invalid_stored_notice_cursor"))?;
            let sequence = sequence
                .parse::<i64>()
                .map_err(|_| refused("manager_invalid_stored_notice_cursor"))?;
            (sequence, job.to_string())
        };
        let mut statement = self.conn.prepare(
            "SELECT candidate.job_id,candidate.epic_id,
                (SELECT notice.source_session_id
                 FROM harness_manager_notices notice
                      INDEXED BY harness_manager_notices_unsettled_job
                 WHERE notice.job_id=candidate.job_id AND notice.retired_at IS NULL
                   AND notice.direction='to_manager' AND notice.settled_at IS NULL
                   AND notice.delivered_at IS NULL
                 ORDER BY notice.sequence DESC LIMIT 1)
             FROM harness_manager_notice_transport_candidates candidate
                  INDEXED BY harness_manager_notice_transport_candidate_page
             WHERE candidate.project_id=?1 AND candidate.manager_session_id=?2
               AND candidate.scope_version=?3
               AND (candidate.first_sequence>?4
                    OR (candidate.first_sequence=?4 AND candidate.job_id>?5))
             ORDER BY candidate.first_sequence,candidate.job_id LIMIT ?6",
        )?;
        let rows = statement
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    after_sequence,
                    after_job,
                    MAX_NOTICE_RECONCILIATION_BATCH
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let next_cursor = if rows.len() == MAX_NOTICE_RECONCILIATION_BATCH as usize {
            let (job, _, _) = rows
                .last()
                .expect("full deferred transport page has a tail");
            let sequence: i64 = self.conn.query_row(
                "SELECT first_sequence FROM harness_manager_notice_transport_candidates
                 WHERE job_id=?1",
                [job],
                |row| row.get(0),
            )?;
            Some(format!("{sequence:020}:{job}"))
        } else {
            None
        };
        let (action_job, _, _) = self.manager_action_watch_identity(config);
        for (job, epic, notice_source) in rows {
            let job =
                Uuid::parse_str(&job).map_err(|_| refused("manager_invalid_stored_identity"))?;
            if self
                .get_scheduled_job(&job)?
                .is_some_and(|scheduled| scheduled.enabled)
            {
                continue;
            }
            if job == action_job {
                self.ensure_manager_action_watch(config, &job.to_string())?;
                self.refresh_manager_notice_job(job)?;
                continue;
            }
            let Some(epic) = epic else {
                self.retire_obsolete_manager_notice_route(config, job)?;
                continue;
            };
            let epic =
                Uuid::parse_str(&epic).map_err(|_| refused("manager_invalid_stored_identity"))?;
            if !self.manager_config_covers_epic(config, epic)? {
                self.retire_obsolete_manager_notice_route(config, job)?;
                continue;
            }
            let lead = match self.manager_lead(config.project_id, epic) {
                Ok(lead) => lead,
                Err(_) => {
                    let Some(source) = notice_source else {
                        self.retire_obsolete_manager_notice_route(config, job)?;
                        continue;
                    };
                    let source = Uuid::parse_str(&source)
                        .map_err(|_| refused("manager_invalid_stored_identity"))?;
                    let Some(source) = self.get_session(source)? else {
                        self.retire_obsolete_manager_notice_route(config, job)?;
                        continue;
                    };
                    if source.parent_id != Some(epic)
                        || !matches!(
                            source.status,
                            SessionStatus::Completed
                                | SessionStatus::Failed
                                | SessionStatus::Interrupted
                                | SessionStatus::WaitingApproval
                                | SessionStatus::Archived
                        )
                    {
                        self.retire_obsolete_manager_notice_route(config, job)?;
                        continue;
                    }
                    source
                }
            };
            let (expected, source, target, _) = self.manager_watch_identity(config, &lead, true)?;
            if expected != job || source == target {
                self.retire_obsolete_manager_notice_route(config, job)?;
                continue;
            }
            self.ensure_manager_watch(config, &lead, true, &job.to_string(), false)?;
            self.refresh_manager_notice_job(job)?;
        }
        self.advance_manager_notice_cursor(
            config,
            "deferred_transports",
            None,
            next_cursor.as_deref(),
        )?;
        Ok(())
    }

    pub(crate) fn enqueue_manager_notice_scope_retirement(
        &self,
        config: &HarnessManagerConfigV1,
        retired_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO harness_manager_notice_scope_retirements(
                project_id,manager_session_id,scope_version,retired_at)
             VALUES(?1,?2,?3,?4)",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                retired_at
            ],
        )?;
        Ok(())
    }

    /// Retire obsolete scope generations in bounded, restart-safe pages.
    /// Scope edits own one durable queue row; scheduler reconciliation drains
    /// it without manufacturing inbox retrieval or settlement evidence.
    pub(crate) fn reconcile_manager_notice_scope_retirements(&self) -> Result<()> {
        let mut statement = self.conn.prepare(
            "SELECT project_id,manager_session_id,scope_version,retired_at
             FROM harness_manager_notice_scope_retirements
                  INDEXED BY harness_manager_notice_scope_retirements_pending
             WHERE completed_at IS NULL
             ORDER BY retired_at,project_id,manager_session_id,scope_version LIMIT ?1",
        )?;
        let retirements = statement
            .query_map([MAX_SCOPE_RETIREMENTS_PER_PASS], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (project, manager, scope, retired_at) in retirements {
            self.reconcile_manager_notice_scope_retirement_owner(
                Uuid::parse_str(&project)
                    .map_err(|_| refused("manager_invalid_stored_identity"))?,
                Uuid::parse_str(&manager)
                    .map_err(|_| refused("manager_invalid_stored_identity"))?,
                scope,
                &retired_at,
            )?;
        }
        Ok(())
    }

    /// Drain one exact old-scope owner with one bounded page per retained
    /// source: at most 256 action receipt obligations and at most 256 notice
    /// rows. The retained owner is completed only after both sources are empty.
    pub(crate) fn reconcile_manager_notice_scope_retirement_owner(
        &self,
        project: Uuid,
        manager: Uuid,
        scope: i64,
        retired_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE harness_manager_action_notice_queue SET retired_at=?4
             WHERE rowid IN (
                SELECT rowid FROM harness_manager_action_notice_queue
                     INDEXED BY harness_manager_action_notice_pending
                WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                  AND reconciled_at IS NULL AND retired_at IS NULL
                ORDER BY queued_at,operation_id,operation_row_version LIMIT ?5
             )",
            params![
                project.to_string(),
                manager.to_string(),
                scope,
                retired_at,
                MAX_NOTICE_RECONCILIATION_BATCH
            ],
        )?;
        self.conn.execute(
            "UPDATE harness_manager_notices SET retired_at=?4
             WHERE id IN (
                SELECT id FROM harness_manager_notices
                     INDEXED BY harness_manager_notices_pending_scope_sequence
                WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                  AND retired_at IS NULL AND settled_at IS NULL
                ORDER BY sequence LIMIT ?5
             )",
            params![
                project.to_string(),
                manager.to_string(),
                scope,
                retired_at,
                MAX_NOTICE_RECONCILIATION_BATCH
            ],
        )?;
        let remaining: bool = self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM harness_manager_action_notice_queue
                     INDEXED BY harness_manager_action_notice_pending
                WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                  AND reconciled_at IS NULL AND retired_at IS NULL
                UNION ALL
                SELECT 1 FROM harness_manager_notices
                     INDEXED BY harness_manager_notices_pending_scope_sequence
                WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                  AND retired_at IS NULL AND settled_at IS NULL
                LIMIT 1
             )",
            params![project.to_string(), manager.to_string(), scope],
            |row| row.get(0),
        )?;
        if !remaining {
            self.conn.execute(
                "UPDATE harness_manager_notice_scope_retirements
                 SET completed_at=?4
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND completed_at IS NULL",
                params![project.to_string(), manager.to_string(), scope, now()],
            )?;
        }
        Ok(())
    }

    /// Retire a transport whose persisted source/target route is no longer
    /// reachable under the current scope. Retirement is distinct from inbox
    /// retrieval: background reconciliation must never claim a reader or
    /// populate retrieval/settlement evidence.
    fn retire_obsolete_manager_notice_route(
        &self,
        config: &HarnessManagerConfigV1,
        job_id: Uuid,
    ) -> Result<()> {
        let retired = now();
        self.conn.execute(
            RETIRE_OBSOLETE_MANAGER_NOTICE_ROUTE_SQL,
            params![
                job_id.to_string(),
                retired,
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                MAX_OBSOLETE_NOTICES_PER_JOB
            ],
        )?;
        self.refresh_manager_notice_job(job_id)
    }

    fn pending_manager_notice_candidates(
        &self,
        config: &HarnessManagerConfigV1,
        direction: &str,
        epic: Option<Uuid>,
        request_id: Option<Uuid>,
    ) -> Result<Vec<StoredNotice>> {
        let mut arguments = vec![
            SqlValue::Text(config.project_id.to_string()),
            SqlValue::Text(config.manager_session_id.to_string()),
            SqlValue::Integer(config.row_version),
            SqlValue::Text(direction.to_owned()),
        ];
        let sql = if let Some(request_id) = request_id {
            arguments.push(SqlValue::Text(request_id.to_string()));
            let epic_filter = if let Some(epic) = epic {
                arguments.push(SqlValue::Text(epic.to_string()));
                " AND epic_id=?6"
            } else {
                ""
            };
            let limit = arguments.len() + 1;
            arguments.push(SqlValue::Integer(MAX_NOTICE_RETRIEVAL_SCAN));
            format!(
                "SELECT * FROM (
                    SELECT {NOTICE_COLUMNS} FROM harness_manager_notices
                         INDEXED BY harness_manager_notices_pending_subject_sequence
                    WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                      AND direction=?4 AND kind='message' AND subject_id=?5
                      AND retired_at IS NULL AND settled_at IS NULL{epic_filter}
                    UNION ALL
                    SELECT {NOTICE_COLUMNS} FROM harness_manager_notices
                         INDEXED BY harness_manager_notices_pending_request_sequence
                    WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                      AND direction=?4 AND kind='message'
                      AND json_extract(state_json,'$.request_id')=?5 AND subject_id<>?5
                      AND retired_at IS NULL AND settled_at IS NULL{epic_filter}
                 ) ORDER BY sequence LIMIT ?{limit}"
            )
        } else {
            let index = if let Some(epic) = epic {
                arguments.push(SqlValue::Text(epic.to_string()));
                "harness_manager_notices_pending_epic_sequence"
            } else {
                "harness_manager_notices_pending_direction_sequence"
            };
            let epic_filter = if epic.is_some() {
                " AND epic_id=?5"
            } else {
                ""
            };
            let limit = arguments.len() + 1;
            arguments.push(SqlValue::Integer(MAX_NOTICE_RETRIEVAL_SCAN));
            format!(
                "SELECT {NOTICE_COLUMNS} FROM harness_manager_notices INDEXED BY {index}
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND direction=?4 AND retired_at IS NULL AND settled_at IS NULL
                   {epic_filter}
                 ORDER BY sequence LIMIT ?{limit}"
            )
        };
        let mut statement = self.conn.prepare(&sql)?;
        Ok(statement
            .query_map(params_from_iter(arguments.iter()), read_notice)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn pending_manager_notice_candidates_remain(
        &self,
        config: &HarnessManagerConfigV1,
        direction: &str,
        epic: Option<Uuid>,
        request_id: Option<Uuid>,
        after: Option<i64>,
    ) -> Result<bool> {
        let Some(after) = after else {
            return Ok(false);
        };
        // One request has one original message and at most 32 replies. The
        // exact subject/request indexes return the whole bounded exchange.
        if request_id.is_some() {
            return Ok(false);
        }
        if let Some(epic) = epic {
            return Ok(self.conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM harness_manager_notices
                         INDEXED BY harness_manager_notices_pending_epic_sequence
                    WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                      AND direction=?4 AND epic_id=?5 AND retired_at IS NULL
                      AND settled_at IS NULL AND sequence>?6
                 )",
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    direction,
                    epic.to_string(),
                    after
                ],
                |row| row.get(0),
            )?);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM harness_manager_notices
                     INDEXED BY harness_manager_notices_pending_direction_sequence
                WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                  AND direction=?4 AND retired_at IS NULL
                  AND settled_at IS NULL AND sequence>?5
             )",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                direction,
                after
            ],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn retrieve_manager_notices(
        &self,
        config: &HarnessManagerConfigV1,
        caller: Uuid,
        is_manager: bool,
        limit: u16,
        request_id: Option<Uuid>,
    ) -> Result<(Vec<HarnessManagerNoticeV1>, bool)> {
        let direction = if is_manager { "to_manager" } else { "to_lead" };
        let epic = if is_manager {
            None
        } else {
            self.manager_session(caller)?.parent_id
        };
        let retrieved = now();
        let mut live = Vec::new();
        let mut jobs = std::collections::HashSet::new();
        let rows = self.pending_manager_notice_candidates(config, direction, epic, request_id)?;
        let last_candidate_sequence = rows.last().map(|row| row.public.sequence);
        for row in rows {
            let scoped = (is_manager && row.public.kind == "action_result")
                || row
                    .public
                    .epic_id
                    .is_some_and(|epic| config.epic_ids.contains(&epic));
            let authorized = scoped
                && self
                    .harness_manager_watch_route(row.job_id)?
                    .is_some_and(|(_, target)| target == caller);
            if !authorized {
                // Each call retires a bounded stale prefix without fabricating
                // retrieval evidence. Repeated retrieval therefore makes
                // forward progress without changing durable mail/gates.
                let changed = self.conn.execute(
                    "UPDATE harness_manager_notices SET retired_at=?2
                     WHERE id=?1 AND retired_at IS NULL AND settled_at IS NULL",
                    params![row.public.notice_id.to_string(), retrieved],
                )?;
                if changed != 1 {
                    return Err(refused("manager_notice_changed"));
                }
                jobs.insert(row.job_id);
                continue;
            }
            live.push(row);
            if live.len() > usize::from(limit) {
                break;
            }
        }
        let unscanned = self.pending_manager_notice_candidates_remain(
            config,
            direction,
            epic,
            request_id,
            last_candidate_sequence,
        )?;
        let more = live.len() > usize::from(limit) || unscanned;
        live.truncate(usize::from(limit));
        for row in &mut live {
            let changed = self.conn.execute(
                "UPDATE harness_manager_notices SET retrieved_at=?2,settled_at=?2
                 WHERE id=?1 AND retired_at IS NULL AND settled_at IS NULL",
                params![row.public.notice_id.to_string(), retrieved],
            )?;
            if changed != 1 {
                return Err(refused("manager_notice_changed"));
            }
            row.public.retrieved_at =
                super::parse_timestamp(&retrieved).map_err(DaemonError::Store)?;
            row.public.settled_at = row.public.retrieved_at;
            jobs.insert(row.job_id);
        }
        for job in jobs {
            self.refresh_manager_notice_job(job)?;
        }
        self.reconcile_deferred_manager_notice_transports(config)?;
        Ok((live.into_iter().map(|row| row.public).collect(), more))
    }

    pub(crate) fn manager_v2_project_decision_retrieval(
        &self,
        config: &HarnessManagerConfigV1,
        row: &mut Value,
    ) -> Result<()> {
        let mut seen = serde_json::Map::new();
        for role in ["manager", "lead"] {
            let key = format!("{role}:{}", row["key"].as_str().unwrap_or_default());
            if let Some(record) = self.manager_v2_record(config, "decision_retrieval", &key)? {
                if record.payload["target_digest"] == row["target_digest"] {
                    seen.insert(role.into(), record.payload);
                }
            }
        }
        row["answer_retrieval"] = Value::Object(seen);
        Ok(())
    }

    /// Called only after the authenticated page has survived its observation
    /// fence. An operator viewing the board does not manufacture agent retrieval.
    pub(crate) fn manager_v2_retrieve_decision_answers(
        &self,
        caller: Uuid,
        result: &mut ManagerInspectionV2,
    ) -> Result<()> {
        if result.section != ManagerInspectSectionV2::Decisions {
            return Ok(());
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        if result.scope_version != config.row_version {
            return Err(refused("manager_v2_scope_changed"));
        }
        let role = if is_manager { "manager" } else { "lead" };
        for row in &mut result.rows {
            if row["type"] != "decision" || row["answer"].is_null() {
                continue;
            }
            let key = row["key"]
                .as_str()
                .ok_or_else(|| refused("manager_v2_decision_changed"))?;
            let live = self
                .manager_v2_record(&config, "decision", &key)?
                .ok_or_else(|| refused("manager_v2_decision_changed"))?;
            if live.payload["target_digest"] != row["target_digest"]
                || Some(live.row_version) != row["row_version"].as_i64()
            {
                return Err(refused("manager_v2_decision_changed"));
            }
            let epic = live
                .epic_id
                .ok_or_else(|| refused("manager_v2_decision_target_required"))?;
            if !config.epic_ids.contains(&epic)
                || (!is_manager && self.manager_lead(config.project_id, epic)?.id != caller)
            {
                return Err(refused("manager_v2_scope_denied"));
            }
            let retrieval_key = format!("{role}:{key}");
            let old = self.manager_v2_record(&config, "decision_retrieval", &retrieval_key)?;
            if !old.as_ref().is_some_and(|r| {
                r.payload["target_digest"] == row["target_digest"]
                    && r.payload["actor_session_id"] == caller.to_string()
            }) {
                self.manager_v2_record_changed(&config,"decision_retrieval",&retrieval_key,Some(epic),
                    &json!({"decision_key":key,"target_digest":row["target_digest"],"actor_session_id":caller,"role":role,"retrieved_at":now()}))?;
            }
            self.manager_v2_project_decision_retrieval(&config, row)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Both v1 terminal reconciliation and v2 semantic reconciliation use this
    /// same signature, so two producers cannot alternately re-arm one notice.
    pub(crate) fn manager_v2_notice_signature(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        to_manager: bool,
    ) -> Result<Option<String>> {
        let Some(policy) = self
            .get_harness_manager_policy(config.project_id)?
            .filter(|p| !p.revoked)
        else {
            return Ok(None);
        };
        let epic = lead
            .parent_id
            .ok_or_else(|| refused("manager_legal_epic_required"))?;
        // The immutable event sequence is a constant-time monotonic witness for
        // all ledger changes in this scope. Exact records are materialized by
        // their own keyset cursors below; signatures never expand the ledger.
        let ledger_sequence: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM harness_manager_v2_events
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version
            ],
            |row| row.get(0),
        )?;
        let question = manager_notice_question_projection(&lead.pending_question)?;
        let sequence: i64 = self.conn.query_row("SELECT COALESCE(MAX(sequence),0) FROM harness_manager_messages WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND epic_id=?4",params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,epic.to_string()],|r|r.get(0))?;
        Ok(Some(fingerprint(
            &json!({"policy_version":policy.row_version,"lead":lead.id,
            "status":lead.status,"stop_reason":lead.stop_reason,"question":question,
            "ledger_sequence":ledger_sequence,
            "decision_generation":self.manager_v2_decision_generation(config,Some(epic))?,
            "global_decision_generation":if to_manager {Some(self.manager_v2_decision_generation(config,None)?)} else {None},
            "mail_sequence":sequence,"to_manager":to_manager}),
        )?))
    }

    fn manager_v2_notice_record_page(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        kind: &str,
    ) -> Result<Vec<super::harness_manager_v2::ManagerRecordV2>> {
        let lane = format!("v2_{kind}");
        let after = self.manager_notice_cursor_after(config, &lane, Some(epic))?;
        let epic_key = if kind == "coordinator_error" {
            None
        } else {
            Some(epic.to_string())
        };
        if super::manager_ledger::is_work_fact(kind) {
            // D19: work facts survive seat moves, so they page by Epic, not seat.
            let keys = self.manager_v2_fact_keys_after(
                config.project_id,
                epic,
                kind,
                &after,
                MAX_NOTICE_RECORDS_PER_EPIC_PASS,
            )?;
            return self.manager_v2_notice_record_keys(config, epic, kind, &lane, &after, keys);
        }
        let mut statement = self.conn.prepare(
            "SELECT record_key FROM harness_manager_v2_records
                 INDEXED BY harness_manager_notice_record_page
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND epic_id IS ?4 AND kind=?5 AND archived=0 AND record_key>?6
             ORDER BY record_key LIMIT ?7",
        )?;
        let keys = statement
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    epic_key,
                    kind,
                    after,
                    MAX_NOTICE_RECORDS_PER_EPIC_PASS
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        self.manager_v2_notice_record_keys(config, epic, kind, &lane, &after, keys)
    }

    fn manager_v2_notice_record_keys(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        kind: &str,
        lane: &str,
        after: &str,
        keys: Vec<String>,
    ) -> Result<Vec<super::harness_manager_v2::ManagerRecordV2>> {
        if keys.is_empty() && !after.is_empty() {
            self.advance_manager_notice_cursor(config, lane, Some(epic), None)?;
            return self.manager_v2_notice_record_page(config, epic, kind);
        }
        self.advance_manager_notice_cursor(
            config,
            lane,
            Some(epic),
            keys.last().map(String::as_str),
        )?;
        keys.into_iter()
            .map(|key| {
                self.manager_v2_record(config, kind, &key)?
                    .ok_or_else(|| refused("manager_v2_record_missing"))
            })
            .collect()
    }

    pub(crate) fn manager_v2_reconcile_notices(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<()> {
        if self
            .get_harness_manager_policy(config.project_id)?
            .is_none_or(|p| p.revoked)
        {
            return Ok(());
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for epic in self.manager_notice_epic_page(config, "v2_epics")? {
            let Ok(lead) = self.manager_lead(config.project_id, epic) else {
                continue;
            };
            if matches!(
                lead.status,
                SessionStatus::Completed
                    | SessionStatus::Failed
                    | SessionStatus::Interrupted
                    | SessionStatus::WaitingApproval
            ) || lead.pending_question.is_some()
            {
                self.record_manager_session_notice(config, &lead)?;
            }
            for kind in ["intent", "work", "request", "coordinator_error"] {
                for record in self.manager_v2_notice_record_page(config, epic, kind)? {
                    self.record_manager_ledger_notice(config, &lead, &record)?;
                }
            }
            let pending_answer = self.manager_v2_unread_operator_answer(config, epic, lead.id)?;
            if pending_answer {
                for decision in self.manager_v2_notice_record_page(config, epic, "decision")? {
                    if decision.payload["delivery"]["state"] != "available_in_scoped_inbox" {
                        continue;
                    }
                    let Some(target_digest) = decision.payload["target_digest"].as_str() else {
                        continue;
                    };
                    self.record_manager_operator_answer_notice(
                        config,
                        &lead,
                        &decision.key,
                        decision.row_version,
                        target_digest,
                    )?;
                }
            }
        }
        self.reconcile_manager_action_notices(config)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn manager_v2_coordinator_error(
        &self,
        project: Uuid,
        error: Option<&str>,
    ) -> Result<bool> {
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(false);
        };
        let old = self.manager_v2_record(&config, "coordinator_error", "current")?;
        if old.is_none() && error.is_none() {
            return Ok(false);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let changed = self.manager_v2_record_changed(&config,"coordinator_error","current",None,
            &json!({"state":if error.is_some(){"blocked"}else{"resolved"},"reason":error,"next_action":if error.is_some(){Some("Repair or subdivide the scoped cohort; durable intent remains in force.")}else{None}}))?;
        tx.commit()?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::manager_coordinator::tests::{fixture, raise_question};
    use crate::store::manager_ledger::LedgerObservation;
    use rsi_common::types::{PendingQuestion, QuestionItem};

    fn update(
        store: &Store,
        actor: Uuid,
        key: &str,
        change: ManagerUpdateV2,
    ) -> Result<ManagerMutationReceiptV2> {
        store.manager_v2_commit_update(
            actor,
            &AgentManagerUpdateRequestV2 {
                fence: ManagerFenceV2 {
                    scope_version: 1,
                    policy_version: 1,
                },
                idempotency_key: key.into(),
                change,
            },
            &LedgerObservation::default(),
        )
    }
    fn decision(
        store: &Store,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        key: &str,
        work: Option<String>,
    ) {
        update(
            store,
            config.manager_session_id,
            &format!("declare-{key}"),
            ManagerUpdateV2::Decision {
                key: key.into(),
                expected_row_version: 0,
                epic_id: lead.parent_id.unwrap(),
                question: format!("Choose {key}"),
                request_id: None,
                work_key: work,
            },
        )
        .unwrap();
    }
    fn answer(
        store: &Store,
        config: &HarnessManagerConfigV1,
        key: &str,
    ) -> Result<ManagerMutationReceiptV2> {
        let d = store.manager_v2_record(config, "decision", key)?.unwrap();
        store.manager_v2_prepare_decision_answer(
            &AnswerHarnessManagerDecisionRequestV2 {
                project_id: config.project_id,
                fence: ManagerFenceV2 {
                    scope_version: 1,
                    policy_version: 1,
                },
                decision_key: key.into(),
                expected_row_version: d.row_version,
                target_digest: d.payload["target_digest"].as_str().unwrap().into(),
                answer: "Use the shared implementation".into(),
                idempotency_key: format!("answer-{key}"),
            },
            |_, _, _, _| unreachable!(),
        )
    }
    fn policy() -> ManagerPolicyV2 {
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::WorkPlan],
            ..Default::default()
        }
    }

    fn action_operation(
        config: &HarnessManagerConfigV1,
        action: ManagerActionV2,
        target_session_id: Option<Uuid>,
        receipt: ManagerActionReceiptV2,
    ) -> ManagerActionOperationV2 {
        ManagerActionOperationV2 {
            project_id: config.project_id,
            manager_session_id: config.manager_session_id,
            scope_version: config.row_version,
            context: crate::store::manager_actions::ManagerActionContextV2 {
                origin: crate::store::manager_actions::ManagerActionOriginV2::Agent {
                    caller: config.manager_session_id,
                },
                request: AgentManagerControlRequestV2 {
                    fence: ManagerFenceV2 {
                        scope_version: config.row_version,
                        policy_version: 1,
                    },
                    idempotency_key: format!("notice-{}", receipt.operation_id),
                    operation: action,
                },
                target_session_id,
                source: None,
                launch: None,
                manager_pause_version: 0,
            },
            receipt,
            effect_started: true,
            settled_fence: None,
            claim_boot_id: None,
        }
    }

    fn insert_pending_test_notice(
        store: &Store,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        job_id: Uuid,
        subject_id: &str,
    ) {
        let recorded = now();
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_notices
                 (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
                  source_session_id,recipient_session_id,kind,subject_id,subject_version,
                  state_json,recorded_at,queued_at)
                 VALUES(?1,?2,?3,?4,?5,?6,'to_manager',?7,?8,'session_state',?9,'1','{}',?10,?10)",
                params![
                    Uuid::new_v4().to_string(),
                    job_id.to_string(),
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    lead.parent_id.unwrap().to_string(),
                    lead.id.to_string(),
                    config.manager_session_id.to_string(),
                    subject_id,
                    recorded
                ],
            )
            .unwrap();
    }

    fn add_scoped_epic(store: &Store, template_lead: &Session) -> Session {
        let template_epic = store
            .get_session(template_lead.parent_id.unwrap())
            .unwrap()
            .unwrap();
        let mut epic = template_epic.clone();
        epic.id = Uuid::new_v4();
        epic.lead_session_id = None;
        epic.title = Some("Deferred transport target".into());
        store.insert_session(&epic).unwrap();
        let mut lead = template_lead.clone();
        lead.id = Uuid::new_v4();
        lead.parent_id = Some(epic.id);
        lead.title = Some("Deferred transport lead".into());
        store.insert_session(&lead).unwrap();
        store.set_lead_session(epic.id, Some(lead.id)).unwrap();
        lead
    }

    fn seed_stale_notice_prefix(
        store: &Store,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        stale_count: usize,
    ) -> Uuid {
        let (live_job, _, _, _) = store.manager_watch_identity(config, lead, true).unwrap();
        let settled = now();
        store
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retrieved_at=?2,settled_at=?2
                 WHERE job_id=?1 AND settled_at IS NULL",
                params![live_job.to_string(), settled],
            )
            .unwrap();
        store.refresh_manager_notice_job(live_job).unwrap();
        for index in 0..stale_count {
            insert_pending_test_notice(
                store,
                config,
                lead,
                Uuid::new_v4(),
                &format!("stale-{index:04}"),
            );
        }
        insert_pending_test_notice(store, config, lead, live_job, "live-after-stale-prefix");
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                [live_job.to_string()],
            )
            .unwrap();
        live_job
    }

    #[test]
    fn manager_v2_board_exposes_current_lead_and_container_control_fences() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let page = store
            .manager_v2_inspect_operator(
                config.project_id,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Topology,
                    ..Default::default()
                },
            )
            .unwrap();
        let epic = page
            .rows
            .iter()
            .find(|r| r["id"] == lead.parent_id.unwrap().to_string())
            .unwrap();
        let expected: ManagerLeadFenceV2 =
            serde_json::from_value(epic["expected"].clone()).unwrap();
        assert_eq!(expected.lead_session_id, Some(lead.id));
        assert_eq!(
            expected.lead_generation,
            store
                .manager_action_lead_fence(lead.parent_id.unwrap())
                .unwrap()
                .lead_generation
        );
        assert_eq!(
            epic["expected_updated_at"],
            json!(
                store
                    .get_session(lead.parent_id.unwrap())
                    .unwrap()
                    .unwrap()
                    .updated_at
            )
        );
        let overview = store
            .manager_v2_inspect_operator(
                config.project_id,
                &AgentManagerInspectRequestV2::default(),
            )
            .unwrap();
        assert!(overview.rows.iter().any(|r| r["type"] == "lead_control"
            && r["expected"]["lead_session_id"] == lead.id.to_string()));
    }

    #[test]
    fn manager_v2_decision_board_refreshes_actual_question_and_protects_daemon_keys() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        raise_question(&store, lead.id, 1);
        let page = store
            .manager_v2_inspect_operator(
                config.project_id,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Decisions,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(page.rows[0]["status"], "pending");
        assert!(
            page.rows[0]["question"]
                .as_str()
                .unwrap()
                .contains("migration")
        );
        let overwrite = update(
            &store,
            config.manager_session_id,
            "overwrite",
            ManagerUpdateV2::Decision {
                key: format!("question:{}", lead.id),
                expected_row_version: 1,
                epic_id: lead.parent_id.unwrap(),
                question: "Change the human question".into(),
                request_id: None,
                work_key: None,
            },
        );
        assert!(
            overwrite
                .unwrap_err()
                .to_string()
                .contains("reserved_decision_key")
        );
        assert_eq!(
            store
                .manager_v2_record(&config, "decision", &format!("question:{}", lead.id))
                .unwrap()
                .unwrap()
                .payload["target_digest"],
            page.rows[0]["target_digest"]
        );
    }

    #[test]
    fn manager_v2_operator_answers_have_attributed_retrieval_without_breaking_pages() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        for key in ["one", "two"] {
            decision(&store, &config, &lead, key, None);
            answer(&store, &config, key).unwrap();
        }
        let query = AgentManagerInspectRequestV2 {
            section: ManagerInspectSectionV2::Decisions,
            limit: 1,
            ..Default::default()
        };
        let operator = store
            .manager_v2_inspect_operator(config.project_id, &query)
            .unwrap();
        assert_eq!(operator.rows[0]["answer_retrieval"], json!({}));
        let mut first = store.manager_v2_inspect(lead.id, &query).unwrap();
        store
            .manager_v2_retrieve_decision_answers(lead.id, &mut first)
            .unwrap();
        assert_eq!(
            first.rows[0]["answer_retrieval"]["lead"]["actor_session_id"],
            lead.id.to_string()
        );
        let second = store
            .manager_v2_inspect(
                lead.id,
                &AgentManagerInspectRequestV2 {
                    cursor: first.next_cursor.clone(),
                    ..query
                },
            )
            .unwrap();
        assert_eq!(second.rows[0]["key"], "two");
        let key = first.rows[0]["key"].as_str().unwrap().to_owned();
        let receipt = store
            .manager_v2_record(&config, "decision_retrieval", &format!("lead:{key}"))
            .unwrap()
            .unwrap();
        store
            .manager_v2_retrieve_decision_answers(lead.id, &mut first)
            .unwrap();
        assert_eq!(
            store
                .manager_v2_record(&config, "decision_retrieval", &format!("lead:{key}"))
                .unwrap()
                .unwrap()
                .row_version,
            receipt.row_version
        );
        assert_eq!(
            store
                .manager_v2_record(&config, "decision", &key)
                .unwrap()
                .unwrap()
                .payload["delivery"]["state"],
            "available_in_scoped_inbox"
        );
    }

    #[test]
    fn manager_v2_operator_answer_refuses_a_changed_work_gate() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        update(
            &store,
            config.manager_session_id,
            "work",
            ManagerUpdateV2::Work {
                key: "work".into(),
                expected_row_version: 0,
                epic_id: lead.parent_id.unwrap(),
                title: "Plan".into(),
                kind: ManagerWorkKindV2::Program,
                priority: 1,
                weight: 1,
                required_gates: vec![ManagerWorkStageV2::Planning],
            },
        )
        .unwrap();
        decision(&store, &config, &lead, "choose", Some("work".into()));
        update(
            &store,
            lead.id,
            "stage",
            ManagerUpdateV2::Stage {
                key: "work".into(),
                expected_row_version: 1,
                stage: ManagerWorkStageV2::Planning,
                state: ManagerStageStateV2::Running,
                note: "A new plan changes the pending gate".into(),
                evidence: None,
            },
        )
        .unwrap();
        assert!(
            answer(&store, &config, "choose")
                .unwrap_err()
                .to_string()
                .contains("decision_target_changed")
        );
        assert_eq!(
            store
                .manager_v2_record(&config, "decision", "choose")
                .unwrap()
                .unwrap()
                .payload["status"],
            "pending"
        );
    }

    #[test]
    fn manager_v2_reconciliation_deduplicates_exact_subject_versions() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        store.manager_v2_reconcile_notices(&config).unwrap();
        let initial: i64 = store
            .conn
            .query_row("SELECT count(*) FROM harness_manager_notices", [], |row| {
                row.get(0)
            })
            .unwrap();
        store.manager_v2_reconcile_notices(&config).unwrap();
        let replayed: i64 = store
            .conn
            .query_row("SELECT count(*) FROM harness_manager_notices", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(replayed, initial, "the same durable versions coalesce");

        decision(&store, &config, &lead, "choose", None);
        answer(&store, &config, "choose").unwrap();
        store.manager_v2_reconcile_notices(&config).unwrap();
        let answer_notice: (String, String, String) = store
            .conn
            .query_row(
                "SELECT kind,subject_id,json_extract(state_json,'$.state')
                 FROM harness_manager_notices WHERE direction='to_lead'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            answer_notice,
            ("operator_answer".into(), "choose".into(), "answered".into())
        );
        store.manager_v2_reconcile_notices(&config).unwrap();
        let answer_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE direction='to_lead' AND kind='operator_answer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(answer_count, 1);
    }

    #[test]
    fn lead_replacement_binds_new_route_and_stale_prefix_cannot_hide_live_notice() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let epic = lead.parent_id.unwrap();
        let (old_job, _, _, _) = store.manager_watch_identity(&config, &lead, true).unwrap();
        let stale_before: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND retired_at IS NULL AND settled_at IS NULL",
                [old_job.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(stale_before > 0);

        let mut replacement = lead.clone();
        replacement.id = Uuid::new_v4();
        replacement.continued_from = None;
        replacement.updated_at += chrono::Duration::nanoseconds(1);
        store.insert_session(&replacement).unwrap();
        store.set_lead_session(epic, Some(replacement.id)).unwrap();
        let (new_job, _, recipient, _) = store
            .manager_watch_identity(&config, &replacement, true)
            .unwrap();
        assert_ne!(new_job, old_job);
        assert_eq!(
            store
                .record_manager_session_notice(&config, &replacement)
                .unwrap(),
            Some(new_job)
        );
        assert_eq!(store.harness_manager_watch_route(old_job).unwrap(), None);
        assert_eq!(
            store.harness_manager_watch_route(new_job).unwrap(),
            Some((replacement.id, recipient))
        );

        let inbox = store
            .manager_inbox(
                config.manager_session_id,
                &rsi_common::harness_manager::AgentManagerInboxRequestV1 {
                    after_sequence: 0,
                    request_id: None,
                    limit: 1,
                },
            )
            .unwrap();
        assert_eq!(inbox.notices.len(), 1);
        assert_eq!(inbox.notices[0].subject_id, replacement.id.to_string());
        assert_eq!(inbox.notices[0].source_session_id, Some(replacement.id));
        let stale_after: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND retired_at IS NULL AND settled_at IS NULL",
                [old_job.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_after, 0);
        let retired_without_retrieval: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND retired_at IS NOT NULL
                   AND retrieved_at IS NULL AND settled_at IS NULL",
                [old_job.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired_without_retrieval, stale_before);
    }

    #[test]
    fn inbox_stale_cleanup_is_bounded_and_reaches_a_live_notice_on_retry() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        seed_stale_notice_prefix(
            &store,
            &config,
            &lead,
            (MAX_NOTICE_RETRIEVAL_SCAN + MAX_NOTICE_RECONCILIATION_BATCH + 1) as usize,
        );

        let request = rsi_common::harness_manager::AgentManagerInboxRequestV1 {
            after_sequence: 0,
            request_id: None,
            limit: 1,
        };
        let first = store
            .manager_inbox(config.manager_session_id, &request)
            .unwrap();
        assert!(first.notices.is_empty());
        assert!(first.more_notices);
        let stale_after_first: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'stale-%' AND retired_at IS NULL
                   AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_after_first, 1);
        let retired_after_first: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'stale-%' AND retired_at IS NOT NULL
                   AND retrieved_at IS NULL AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retired_after_first,
            MAX_NOTICE_RETRIEVAL_SCAN + MAX_NOTICE_RECONCILIATION_BATCH
        );

        let second = store
            .manager_inbox(config.manager_session_id, &request)
            .unwrap();
        assert_eq!(second.notices.len(), 1);
        assert_eq!(second.notices[0].subject_id, "live-after-stale-prefix");
        let stale_after_second: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'stale-%' AND retired_at IS NULL
                   AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_after_second, 0);
        let retired_after_second: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'stale-%' AND retired_at IS NOT NULL
                   AND retrieved_at IS NULL AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired_after_second, 513);
    }

    #[test]
    fn stale_route_retirement_v118_has_job_bounded_steps_and_restart_order() {
        use rusqlite::StatementStatus;

        let mut measured_steps = Vec::new();
        for unrelated_count in [1_000, 10_000] {
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("retirement.sqlite");
            let store = Store::open(&database).unwrap();
            let (config, lead) = fixture(&store, policy());
            let (live_job, _, _, _) = store.manager_watch_identity(&config, &lead, true).unwrap();
            let obsolete_job = Uuid::new_v4();
            let tx = store.conn.unchecked_transaction().unwrap();
            for index in 0..unrelated_count {
                insert_pending_test_notice(
                    &store,
                    &config,
                    &lead,
                    live_job,
                    &format!("prefix-{index}"),
                );
            }
            // Retained, already-final rows for the target must not consume a live page.
            for index in 0..16 {
                insert_pending_test_notice(
                    &store,
                    &config,
                    &lead,
                    obsolete_job,
                    &format!("old-{index}"),
                );
            }
            store
                .conn
                .execute(
                    "UPDATE harness_manager_notices SET retired_at=?2 WHERE job_id=?1",
                    params![obsolete_job.to_string(), now()],
                )
                .unwrap();
            for index in 0..33 {
                insert_pending_test_notice(
                    &store,
                    &config,
                    &lead,
                    obsolete_job,
                    &format!("target-{index:02}"),
                );
            }
            // Delivered is not retrieved: this notice must still be retired truthfully.
            store
                .conn
                .execute(
                    "UPDATE harness_manager_notices SET delivered_at=?2
                 WHERE job_id=?1 AND subject_id='target-00'",
                    params![obsolete_job.to_string(), now()],
                )
                .unwrap();
            tx.commit().unwrap();
            let unrelated_before: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND retired_at IS NULL AND settled_at IS NULL",
                    [live_job.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            // Exercise a populated exact-V117 upgrade, not just a fresh V118 store.
            crate::store::tests::rewind_store_to_schema_version(&store.conn, 117);
            drop(store);
            let store = Store::open(&database).unwrap();
            let retired = now();
            let arguments = params![
                obsolete_job.to_string(),
                retired,
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                MAX_OBSOLETE_NOTICES_PER_JOB,
            ];
            let plan = store
                .conn
                .prepare(&format!(
                    "EXPLAIN QUERY PLAN {RETIRE_OBSOLETE_MANAGER_NOTICE_ROUTE_SQL}"
                ))
                .unwrap()
                .query_map(arguments, |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(plan.iter().any(|detail| detail.contains(
                "harness_manager_notices_pending_job_route_sequence (job_id=? AND project_id=? AND manager_session_id=? AND scope_version=? AND direction=?)"
            )), "retirement must seek the exact job/route: {plan:?}");
            let mut statement = store
                .conn
                .prepare(RETIRE_OBSOLETE_MANAGER_NOTICE_ROUTE_SQL)
                .unwrap();
            assert_eq!(statement.execute(arguments).unwrap(), 16);
            let steps = statement.get_status(StatementStatus::VmStep);
            assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
            assert_eq!(statement.get_status(StatementStatus::Sort), 0);
            assert!(
                steps > 0 && steps < 20_000,
                "unexpected retirement work: {steps}"
            );
            measured_steps.push(steps);
            eprintln!(
                "TW-019 unrelated={unrelated_count}, retired=16, vm_steps={steps}, plan={plan:?}"
            );
            drop(statement);
            drop(store);

            for expected_retired in [16, 32, 33] {
                let store = Store::open(&database).unwrap();
                if expected_retired > 16 {
                    store
                        .retire_obsolete_manager_notice_route(&config, obsolete_job)
                        .unwrap();
                }
                let rows = store
                    .conn
                    .prepare(
                        "SELECT subject_id,retired_at IS NOT NULL,retrieved_at,settled_at
                     FROM harness_manager_notices WHERE job_id=?1 AND subject_id LIKE 'target-%'
                     ORDER BY sequence",
                    )
                    .unwrap()
                    .query_map([obsolete_job.to_string()], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                assert_eq!(rows.len(), 33);
                for (index, (subject, retired, retrieved, settled)) in rows.into_iter().enumerate()
                {
                    assert_eq!(subject, format!("target-{index:02}"));
                    assert_eq!(retired, index < expected_retired);
                    assert_eq!((retrieved, settled), (None, None));
                }
                let unrelated_after: i64 = store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM harness_manager_notices
                     WHERE job_id=?1 AND retired_at IS NULL AND settled_at IS NULL",
                        [live_job.to_string()],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(unrelated_after, unrelated_before);
            }
        }
        assert!(
            measured_steps[1] <= measured_steps[0] + 64,
            "tenfold unrelated growth must not grow retirement scan work: {measured_steps:?}"
        );
    }

    #[test]
    fn deferred_recovery_retires_stale_jobs_in_bounded_forward_batches() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let live_job = seed_stale_notice_prefix(
            &store,
            &config,
            &lead,
            (MAX_NOTICE_RECONCILIATION_BATCH + 1) as usize,
        );

        store
            .reconcile_deferred_manager_notice_transports(&config)
            .unwrap();
        let stale_after_first: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'stale-%' AND retired_at IS NULL
                   AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_after_first, 1);
        let retired_after_first: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'stale-%' AND retired_at IS NOT NULL
                   AND retrieved_at IS NULL AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired_after_first, MAX_NOTICE_RECONCILIATION_BATCH);
        assert_eq!(
            store.get_scheduled_job(&live_job).unwrap().unwrap().enabled,
            false,
            "the fixed first page must not skip its remaining stale candidate"
        );

        store
            .reconcile_deferred_manager_notice_transports(&config)
            .unwrap();
        let stale_after_second: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'stale-%' AND retired_at IS NULL
                   AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_after_second, 0);
        assert!(store.get_scheduled_job(&live_job).unwrap().unwrap().enabled);
    }

    #[test]
    fn deferred_transport_discovery_skips_repeated_enabled_job_prefix() {
        let store = Store::open_in_memory().unwrap();
        let (old_config, first_lead) = fixture(&store, policy());
        let second_lead = add_scoped_epic(&store, &first_lead);
        let config = store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: old_config.project_id,
                    session_id: old_config.manager_session_id,
                    epic_ids: Some(vec![
                        first_lead.parent_id.unwrap(),
                        second_lead.parent_id.unwrap(),
                    ]),
                    group_ids: Vec::new(),
                    expected_row_version: old_config.row_version,
                },
            )
            .unwrap();
        let (first_job, _, _, _) = store
            .manager_watch_identity(&config, &first_lead, true)
            .unwrap();
        let (second_job, _, _, _) = store
            .manager_watch_identity(&config, &second_lead, true)
            .unwrap();
        let settled = now();
        store
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retrieved_at=?2,settled_at=?2
                 WHERE job_id=?1 AND settled_at IS NULL",
                params![second_job.to_string(), settled],
            )
            .unwrap();
        store.refresh_manager_notice_job(second_job).unwrap();
        for index in 0..(MAX_NOTICE_RECONCILIATION_BATCH + 32) {
            insert_pending_test_notice(
                &store,
                &config,
                &first_lead,
                first_job,
                &format!("enabled-prefix-{index:04}"),
            );
        }
        let mut transition = second_lead.clone();
        transition.status = SessionStatus::Failed;
        transition.updated_at += chrono::Duration::nanoseconds(1);
        store
            .record_manager_session_notice(&config, &transition)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                [second_job.to_string()],
            )
            .unwrap();
        assert!(
            store
                .get_scheduled_job(&first_job)
                .unwrap()
                .unwrap()
                .enabled
        );
        assert!(
            !store
                .get_scheduled_job(&second_job)
                .unwrap()
                .unwrap()
                .enabled
        );

        store
            .reconcile_deferred_manager_notice_transports(&config)
            .unwrap();

        assert!(
            store
                .get_scheduled_job(&second_job)
                .unwrap()
                .unwrap()
                .enabled,
            "one candidate row per job must advance past any number of notices for an enabled job"
        );
    }

    #[test]
    fn deferred_transport_discovery_pages_past_distinct_enabled_live_candidates() {
        use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};

        let store = Store::open_in_memory().unwrap();
        let (old_config, first_lead) = fixture(&store, policy());
        let second_lead = add_scoped_epic(&store, &first_lead);
        let config = store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: old_config.project_id,
                    session_id: old_config.manager_session_id,
                    epic_ids: Some(vec![
                        first_lead.parent_id.unwrap(),
                        second_lead.parent_id.unwrap(),
                    ]),
                    group_ids: Vec::new(),
                    expected_row_version: old_config.row_version,
                },
            )
            .unwrap();
        let (target_job, _, _, _) = store
            .manager_watch_identity(&config, &second_lead, true)
            .unwrap();
        let settled = now();
        store
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retrieved_at=?2,settled_at=?2
                 WHERE job_id=?1 AND settled_at IS NULL",
                params![target_job.to_string(), settled],
            )
            .unwrap();
        store.refresh_manager_notice_job(target_job).unwrap();

        let mut prefix_jobs = Vec::new();
        for index in 0..=MAX_NOTICE_RECONCILIATION_BATCH {
            let timestamp = Utc::now();
            let job = ScheduledJob {
                id: Uuid::new_v4(),
                name: format!("enabled live candidate {index:04}"),
                message: String::new(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::EverySeconds(60),
                    anchor: timestamp,
                },
                last_fired_at: None,
                next_fire_at: timestamp,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: Some(config.project_id),
                created_at: timestamp,
                updated_at: timestamp,
                wake_mode: WakeMode::OnTerminal(first_lead.id),
                // Each live candidate targets a distinct recipient, so this
                // current-schema state respects the 64-watch per-recipient cap.
                wake_session_id: Some(Uuid::new_v4()),
            };
            store.insert_scheduled_job(&job).unwrap();
            insert_pending_test_notice(
                &store,
                &config,
                &first_lead,
                job.id,
                &format!("distinct-enabled-prefix-{index:04}"),
            );
            prefix_jobs.push(job.id);
        }

        let mut transition = second_lead.clone();
        transition.status = SessionStatus::Failed;
        transition.updated_at += chrono::Duration::nanoseconds(1);
        store
            .record_manager_session_notice(&config, &transition)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                [target_job.to_string()],
            )
            .unwrap();

        store
            .reconcile_deferred_manager_notice_transports(&config)
            .unwrap();
        assert!(
            !store
                .get_scheduled_job(&target_job)
                .unwrap()
                .unwrap()
                .enabled,
            "pass one must advance only through the fixed 256-candidate page"
        );
        assert!(
            !store
                .manager_notice_cursor_after(&config, "deferred_transports", None)
                .unwrap()
                .is_empty(),
            "a full live page must persist its forward cursor"
        );

        store
            .reconcile_deferred_manager_notice_transports(&config)
            .unwrap();
        assert!(
            store
                .get_scheduled_job(&target_job)
                .unwrap()
                .unwrap()
                .enabled,
            "pass two must reach and enable the later recoverable target"
        );
        assert!(
            prefix_jobs.iter().all(|job_id| {
                store
                    .get_scheduled_job(job_id)
                    .unwrap()
                    .is_some_and(|job| job.enabled)
            }),
            "cursor progress must not disable valid live prefix owners"
        );
        let live_prefix_notices: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'distinct-enabled-prefix-%'
                   AND retired_at IS NULL AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live_prefix_notices, MAX_NOTICE_RECONCILIATION_BATCH + 1);
    }

    #[test]
    fn deferred_transport_discovery_progresses_past_more_than_page_of_dead_candidates() {
        let store = Store::open_in_memory().unwrap();
        let (old_config, first_lead) = fixture(&store, policy());
        let second_lead = add_scoped_epic(&store, &first_lead);
        let config = store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: old_config.project_id,
                    session_id: old_config.manager_session_id,
                    epic_ids: Some(vec![
                        first_lead.parent_id.unwrap(),
                        second_lead.parent_id.unwrap(),
                    ]),
                    group_ids: Vec::new(),
                    expected_row_version: old_config.row_version,
                },
            )
            .unwrap();
        let (second_job, _, _, _) = store
            .manager_watch_identity(&config, &second_lead, true)
            .unwrap();
        let settled = now();
        store
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retrieved_at=?2,settled_at=?2
                 WHERE job_id=?1 AND settled_at IS NULL",
                params![second_job.to_string(), settled],
            )
            .unwrap();
        store.refresh_manager_notice_job(second_job).unwrap();

        for index in 0..(MAX_NOTICE_RECONCILIATION_BATCH + 32) {
            let dead_job = Uuid::new_v4();
            insert_pending_test_notice(
                &store,
                &config,
                &first_lead,
                dead_job,
                &format!("dead-candidate-{index:04}"),
            );
            store
                .conn
                .execute(
                    "UPDATE harness_manager_notices SET retrieved_at=?2,settled_at=?2
                     WHERE job_id=?1 AND settled_at IS NULL",
                    params![dead_job.to_string(), settled],
                )
                .unwrap();
        }
        let retained_dead_notices: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'dead-candidate-%' AND settled_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_dead_notices, MAX_NOTICE_RECONCILIATION_BATCH + 32);
        let live_candidates_for_dead_history: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notice_transport_candidates candidate
                 JOIN harness_manager_notices notice ON notice.job_id=candidate.job_id
                 WHERE notice.subject_id LIKE 'dead-candidate-%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live_candidates_for_dead_history, 0);

        let mut transition = second_lead.clone();
        transition.status = SessionStatus::Failed;
        transition.updated_at += chrono::Duration::nanoseconds(1);
        store
            .record_manager_session_notice(&config, &transition)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                [second_job.to_string()],
            )
            .unwrap();

        store
            .reconcile_deferred_manager_notice_transports(&config)
            .unwrap();

        assert!(
            store
                .get_scheduled_job(&second_job)
                .unwrap()
                .unwrap()
                .enabled,
            "retained audit history must not consume the fixed live-candidate page"
        );
    }

    #[test]
    fn deferred_recovery_bounds_cleanup_within_one_stale_job() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let (live_job, _, _, _) = store.manager_watch_identity(&config, &lead, true).unwrap();
        let settled = now();
        store
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retrieved_at=?2,settled_at=?2
                 WHERE job_id=?1 AND settled_at IS NULL",
                params![live_job.to_string(), settled],
            )
            .unwrap();
        store.refresh_manager_notice_job(live_job).unwrap();
        let stale_job = Uuid::new_v4();
        for index in 0..=MAX_OBSOLETE_NOTICES_PER_JOB {
            insert_pending_test_notice(
                &store,
                &config,
                &lead,
                stale_job,
                &format!("stale-shared-{index:02}"),
            );
        }
        insert_pending_test_notice(
            &store,
            &config,
            &lead,
            live_job,
            "live-after-shared-stale-job",
        );
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                [live_job.to_string()],
            )
            .unwrap();

        store
            .reconcile_deferred_manager_notice_transports(&config)
            .unwrap();

        let stale_remaining: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND retired_at IS NULL AND settled_at IS NULL",
                [stale_job.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_remaining, 1);
        let retired_without_retrieval: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND retired_at IS NOT NULL
                   AND retrieved_at IS NULL AND settled_at IS NULL",
                [stale_job.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired_without_retrieval, MAX_OBSOLETE_NOTICES_PER_JOB);
        assert!(
            store.get_scheduled_job(&live_job).unwrap().unwrap().enabled,
            "bounded stale cleanup must not hide the following live transport"
        );
    }

    #[test]
    fn inbox_settles_exact_notice_without_clearing_question_or_operator_answer() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        raise_question(&store, lead.id, 7);
        store.reconcile_harness_manager_watches().unwrap();
        decision(&store, &config, &lead, "approval", None);
        answer(&store, &config, "approval").unwrap();
        store.manager_v2_reconcile_notices(&config).unwrap();

        let session_versions: i64 = store
            .conn
            .query_row(
                "SELECT count(DISTINCT subject_version) FROM harness_manager_notices
                 WHERE kind='session_state' AND subject_id=?1",
                [lead.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            session_versions, 2,
            "a later question/status event remains distinct from the initial completion"
        );

        let inbox = store
            .manager_inbox(config.manager_session_id, &Default::default())
            .unwrap();
        let session_notice = inbox
            .notices
            .iter()
            .find(|notice| {
                notice.kind == "session_state" && notice.state["status"] == "WaitingApproval"
            })
            .unwrap();
        assert_eq!(session_notice.subject_id, lead.id.to_string());
        assert_eq!(session_notice.state["session_id"], lead.id.to_string());
        assert_eq!(session_notice.state["status"], "WaitingApproval");
        assert_eq!(session_notice.retrieved_at, session_notice.settled_at);
        assert!(
            store
                .manager_inbox(config.manager_session_id, &Default::default())
                .unwrap()
                .notices
                .is_empty(),
            "retrieval settles the exact notice once"
        );

        let persisted = store.get_session(lead.id).unwrap().unwrap();
        assert_eq!(persisted.status, SessionStatus::WaitingApproval);
        assert!(persisted.pending_question.is_some());
        assert_eq!(
            store
                .manager_v2_record(&config, "decision", "approval")
                .unwrap()
                .unwrap()
                .payload["delivery"]["state"],
            "available_in_scoped_inbox"
        );
    }

    #[test]
    fn action_notice_binds_operation_target_and_terminal_receipt() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let receipt = ManagerActionReceiptV2 {
            operation_id: Uuid::new_v4(),
            state: ManagerActionStateV2::Succeeded,
            action_kind: ManagerActionKindV2::PauseLead,
            target_type: ManagerActionTargetTypeV2::EpicLead,
            row_version: 3,
            target_session_id: Some(lead.id),
            outcome: Some("lead_committed".into()),
            result: Some(ManagerActionResultV2::LeadPaused),
            deduplicated: false,
            operator_result: None,
        };
        let operation = action_operation(
            &config,
            ManagerActionV2::PauseLead {
                epic_id: lead.parent_id.unwrap(),
                expected: store
                    .manager_action_lead_fence(lead.parent_id.unwrap())
                    .unwrap(),
                reason: "test action notice".into(),
            },
            Some(lead.id),
            receipt.clone(),
        );
        store
            .record_manager_action_notice(&config, &operation)
            .unwrap();
        let inbox = store
            .manager_inbox(config.manager_session_id, &Default::default())
            .unwrap();
        let notice = inbox
            .notices
            .iter()
            .find(|notice| notice.kind == "action_result")
            .unwrap();
        assert_eq!(notice.subject_id, receipt.operation_id.to_string());
        assert_eq!(notice.epic_id, lead.parent_id);
        assert_eq!(notice.subject_version, "3");
        assert_eq!(notice.source_session_id, Some(lead.id));
        assert_eq!(notice.state["state"], "succeeded");
        assert_eq!(notice.state["outcome"], "lead_committed");
    }

    #[test]
    fn project_wide_action_notice_uses_manager_transport_without_fabricating_epic() {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let target = Uuid::new_v4();
        let receipt = ManagerActionReceiptV2 {
            operation_id: Uuid::new_v4(),
            state: ManagerActionStateV2::Succeeded,
            action_kind: ManagerActionKindV2::CreateContainer,
            target_type: ManagerActionTargetTypeV2::GroupContainer,
            row_version: 3,
            target_session_id: Some(target),
            outcome: Some("container_committed".into()),
            result: Some(ManagerActionResultV2::ContainerCommitted { lead_state: None }),
            deduplicated: false,
            operator_result: None,
        };
        let operation = action_operation(
            &config,
            ManagerActionV2::CreateContainer {
                parent_id: None,
                kind: SessionKind::Group,
                name: "New group".into(),
                tags: Vec::new(),
            },
            Some(target),
            receipt.clone(),
        );

        let job = store
            .record_manager_action_notice(&config, &operation)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.harness_manager_watch_route(job).unwrap(),
            Some((config.manager_session_id, config.manager_session_id))
        );
        let inbox = store
            .manager_inbox(config.manager_session_id, &Default::default())
            .unwrap();
        let notice = inbox
            .notices
            .iter()
            .find(|notice| notice.subject_id == receipt.operation_id.to_string())
            .unwrap();
        assert_eq!(notice.epic_id, None);
        assert_eq!(notice.source_session_id, Some(target));
        assert_eq!(notice.state["outcome"], "container_committed");
    }

    #[test]
    fn vacant_lead_action_notice_does_not_depend_on_the_replacement_route() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let epic = lead.parent_id.unwrap();
        let expected = store.manager_action_lead_fence(epic).unwrap();
        store.set_lead_session(epic, None).unwrap();
        let receipt = ManagerActionReceiptV2 {
            operation_id: Uuid::new_v4(),
            state: ManagerActionStateV2::Succeeded,
            action_kind: ManagerActionKindV2::AssignLead,
            target_type: ManagerActionTargetTypeV2::EpicLead,
            row_version: 3,
            target_session_id: None,
            outcome: Some("lead_committed".into()),
            result: Some(ManagerActionResultV2::LeadUnassigned),
            deduplicated: false,
            operator_result: None,
        };
        let operation = action_operation(
            &config,
            ManagerActionV2::AssignLead {
                epic_id: epic,
                expected,
                session_id: None,
            },
            None,
            receipt.clone(),
        );

        store
            .record_manager_action_notice(&config, &operation)
            .unwrap();
        let inbox = store
            .manager_inbox(config.manager_session_id, &Default::default())
            .unwrap();
        let notice = inbox
            .notices
            .iter()
            .find(|notice| notice.subject_id == receipt.operation_id.to_string())
            .unwrap();
        assert_eq!(notice.epic_id, Some(epic));
        assert_eq!(notice.state["state"], "succeeded");
    }

    #[test]
    fn action_reconciliation_queue_uses_its_bounded_pending_index() {
        let store = Store::open_in_memory().unwrap();
        let details = store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT operation_id,operation_row_version,receipt_json
                 FROM harness_manager_action_notice_queue
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND reconciled_at IS NULL AND retired_at IS NULL
                 ORDER BY queued_at,operation_id,operation_row_version LIMIT ?4",
            )
            .unwrap()
            .query_map(
                params![
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    1,
                    MAX_NOTICE_RECONCILIATION_BATCH
                ],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("harness_manager_action_notice_pending")),
            "the reconciliation page must start from the partial pending index: {details:?}"
        );
        assert!(
            details.iter().all(|detail| !detail.contains("TEMP B-TREE")),
            "the pending index must satisfy the bounded order: {details:?}"
        );
    }

    #[test]
    fn mixed_kind_queue_prefix_retires_in_one_page_then_recovers_exact_action() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(
            &store,
            ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![ManagerCapabilityV2::LeadControl],
                ..Default::default()
            },
        );
        let policy_version = store
            .get_harness_manager_policy(config.project_id)
            .unwrap()
            .unwrap()
            .row_version;
        let poison_queued_at = "2000-01-01T00:00:00.000000000Z";
        for index in 0..MAX_NOTICE_RECONCILIATION_BATCH {
            let operation_id = store
                .manager_v2_save_receipt(
                    &config,
                    None,
                    policy_version,
                    "configure_policy",
                    &format!("mixed-kind-prefix-{index}"),
                    &json!({"index":index}),
                    &json!({"row_version":index + 1}),
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO harness_manager_action_notice_queue(
                         operation_id,project_id,manager_session_id,scope_version,
                         operation_row_version,receipt_json,queued_at)
                     SELECT id,project_id,manager_session_id,scope_version,
                            row_version,outcome_json,?2
                     FROM harness_manager_v2_operations WHERE id=?1",
                    params![operation_id.to_string(), poison_queued_at],
                )
                .unwrap();
        }

        let queued = store
            .enqueue_manager_action(
                crate::store::manager_actions::ManagerActionOriginV2::Agent {
                    caller: config.manager_session_id,
                },
                AgentManagerControlRequestV2 {
                    fence: ManagerFenceV2 {
                        scope_version: config.row_version,
                        policy_version,
                    },
                    idempotency_key: "mixed-kind-exact-action".into(),
                    operation: ManagerActionV2::PauseLead {
                        epic_id: lead.parent_id.unwrap(),
                        expected: store
                            .manager_action_lead_fence(lead.parent_id.unwrap())
                            .unwrap(),
                        reason: "recover behind retained non-action rows".into(),
                    },
                },
            )
            .unwrap();
        let claim = store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap();
        let receipt = store
            .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "test_terminal")
            .unwrap();
        assert_eq!(receipt.operation_id, queued.operation_id);

        store.manager_v2_reconcile_notices(&config).unwrap();
        let first_pass: (i64, i64, i64) = store
            .conn
            .query_row(
                "SELECT
                   (SELECT count(*) FROM harness_manager_action_notice_queue
                     WHERE queued_at=?1 AND retired_at IS NOT NULL
                       AND reconciled_at IS NULL),
                   (SELECT count(*) FROM harness_manager_action_notice_queue
                     WHERE operation_id=?2 AND operation_row_version=?3
                       AND reconciled_at IS NULL AND retired_at IS NULL),
                   (SELECT count(*) FROM harness_manager_notices
                     WHERE kind='action_result' AND subject_id=?2
                       AND subject_version=?4)",
                params![
                    poison_queued_at,
                    receipt.operation_id.to_string(),
                    receipt.row_version,
                    receipt.row_version.to_string()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            first_pass,
            (MAX_NOTICE_RECONCILIATION_BATCH, 1, 0),
            "one bounded pass retires the invalid prefix and preserves the exact action obligation"
        );

        store.manager_v2_reconcile_notices(&config).unwrap();
        let recovered: (i64, i64, i64) = store
            .conn
            .query_row(
                "SELECT
                   (SELECT count(*) FROM harness_manager_action_notice_queue
                     WHERE queued_at=?1 AND retired_at IS NOT NULL
                       AND reconciled_at IS NULL),
                   (SELECT count(*) FROM harness_manager_action_notice_queue
                     WHERE operation_id=?2 AND operation_row_version=?3
                       AND reconciled_at IS NOT NULL AND retired_at IS NULL),
                   (SELECT count(*) FROM harness_manager_notices
                     WHERE kind='action_result' AND subject_id=?2
                       AND subject_version=?4)",
                params![
                    poison_queued_at,
                    receipt.operation_id.to_string(),
                    receipt.row_version,
                    receipt.row_version.to_string()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(recovered, (MAX_NOTICE_RECONCILIATION_BATCH, 1, 1));
    }

    #[test]
    fn notice_candidate_queries_use_bounded_ordered_access_paths() {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4().to_string();
        let manager = Uuid::new_v4().to_string();
        let epic = Uuid::new_v4().to_string();
        let request = Uuid::new_v4().to_string();
        let manager_inbox = store
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN
                 SELECT {NOTICE_COLUMNS} FROM harness_manager_notices
                      INDEXED BY harness_manager_notices_pending_direction_sequence
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND direction='to_manager' AND retired_at IS NULL AND settled_at IS NULL
                 ORDER BY sequence LIMIT ?4"
            ))
            .unwrap()
            .query_map(
                params![project, manager, 1, MAX_NOTICE_RETRIEVAL_SCAN],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            manager_inbox.iter().any(|detail| detail
                .contains("harness_manager_notices_pending_direction_sequence")),
            "manager inbox must start from its ordered live-scope index: {manager_inbox:?}"
        );
        assert!(
            manager_inbox
                .iter()
                .all(|detail| !detail.contains("TEMP B-TREE")),
            "manager inbox ordering must not sort retained history: {manager_inbox:?}"
        );

        let lead_inbox = store
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN
                 SELECT {NOTICE_COLUMNS} FROM harness_manager_notices
                      INDEXED BY harness_manager_notices_pending_epic_sequence
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND direction='to_lead' AND epic_id=?4
                   AND retired_at IS NULL AND settled_at IS NULL
                 ORDER BY sequence LIMIT ?5"
            ))
            .unwrap()
            .query_map(
                params![project, manager, 1, epic, MAX_NOTICE_RETRIEVAL_SCAN],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            lead_inbox
                .iter()
                .any(|detail| detail.contains("harness_manager_notices_pending_epic_sequence")),
            "lead inbox must start from its ordered live-Epic index: {lead_inbox:?}"
        );
        assert!(
            lead_inbox
                .iter()
                .all(|detail| !detail.contains("TEMP B-TREE")),
            "lead inbox ordering must not sort retained history: {lead_inbox:?}"
        );

        let deferred = store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT candidate.job_id,candidate.epic_id
                 FROM harness_manager_notice_transport_candidates candidate
                      INDEXED BY harness_manager_notice_transport_candidate_page
                 WHERE candidate.project_id=?1 AND candidate.manager_session_id=?2
                   AND candidate.scope_version=?3
                   AND (candidate.first_sequence>?4
                        OR (candidate.first_sequence=?4 AND candidate.job_id>?5))
                 ORDER BY candidate.first_sequence,candidate.job_id LIMIT ?6",
            )
            .unwrap()
            .query_map(
                params![project, manager, 1, 0, "", MAX_NOTICE_RECONCILIATION_BATCH],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            deferred
                .iter()
                .any(|detail| detail.contains("harness_manager_notice_transport_candidate_page")),
            "deferred recovery must start from its one-row-per-job candidate index: {deferred:?}"
        );
        assert!(
            deferred
                .iter()
                .all(|detail| !detail.contains("TEMP B-TREE")),
            "deferred recovery must not group or sort retained history: {deferred:?}"
        );

        let request_lookup = store
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN
                 SELECT * FROM (
                    SELECT {NOTICE_COLUMNS} FROM harness_manager_notices
                         INDEXED BY harness_manager_notices_pending_subject_sequence
                    WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                      AND direction='to_manager' AND kind='message' AND subject_id=?4
                      AND retired_at IS NULL AND settled_at IS NULL
                    UNION ALL
                    SELECT {NOTICE_COLUMNS} FROM harness_manager_notices
                         INDEXED BY harness_manager_notices_pending_request_sequence
                    WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                      AND direction='to_manager' AND kind='message'
                      AND json_extract(state_json,'$.request_id')=?4 AND subject_id<>?4
                      AND retired_at IS NULL AND settled_at IS NULL
                 ) ORDER BY sequence LIMIT ?5"
            ))
            .unwrap()
            .query_map(
                params![project, manager, 1, request, MAX_NOTICE_RETRIEVAL_SCAN],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for index in [
            "harness_manager_notices_pending_subject_sequence",
            "harness_manager_notices_pending_request_sequence",
        ] {
            assert!(
                request_lookup.iter().any(|detail| detail.contains(index)),
                "request retrieval must use exact indexed branches: {request_lookup:?}"
            );
        }
    }

    #[test]
    fn action_reconciliation_retains_each_terminal_version_for_one_operation() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(
            &store,
            ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![ManagerCapabilityV2::LeadControl],
                ..Default::default()
            },
        );
        let policy_version = store
            .get_harness_manager_policy(config.project_id)
            .unwrap()
            .unwrap()
            .row_version;
        let queued = store
            .enqueue_manager_action(
                crate::store::manager_actions::ManagerActionOriginV2::Agent {
                    caller: config.manager_session_id,
                },
                AgentManagerControlRequestV2 {
                    fence: ManagerFenceV2 {
                        scope_version: config.row_version,
                        policy_version,
                    },
                    idempotency_key: "terminal-version-transition".into(),
                    operation: ManagerActionV2::PauseLead {
                        epic_id: lead.parent_id.unwrap(),
                        expected: store
                            .manager_action_lead_fence(lead.parent_id.unwrap())
                            .unwrap(),
                        reason: "exercise terminal receipt versions".into(),
                    },
                },
            )
            .unwrap();
        let claim = store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap();
        assert_eq!(claim.id(), queued.operation_id);
        let uncertain = store
            .finish_manager_action(
                &claim,
                ManagerActionStateV2::Uncertain,
                "execution_owner_lost_unconfirmed",
            )
            .unwrap();
        let operation = store
            .manager_action_operation(uncertain.operation_id)
            .unwrap()
            .unwrap();
        store
            .record_manager_action_notice(&config, &operation)
            .unwrap();

        let final_receipt = ManagerActionReceiptV2 {
            state: ManagerActionStateV2::Failed,
            row_version: uncertain.row_version + 1,
            outcome: Some("manager_successor_checked_settlement".into()),
            ..uncertain.clone()
        };
        store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations
                 SET state='failed',row_version=?2,outcome_json=?3,updated_at=?4
                 WHERE id=?1 AND state='uncertain'",
                params![
                    final_receipt.operation_id.to_string(),
                    final_receipt.row_version,
                    serde_json::to_string(&final_receipt).unwrap(),
                    now()
                ],
            )
            .unwrap();
        store.manager_v2_reconcile_notices(&config).unwrap();

        let mut statement = store
            .conn
            .prepare(
                "SELECT subject_version,json_extract(state_json,'$.state')
                 FROM harness_manager_notices
                 WHERE kind='action_result' AND subject_id=?1 ORDER BY sequence",
            )
            .unwrap();
        let versions = statement
            .query_map([final_receipt.operation_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            versions,
            vec![
                (uncertain.row_version.to_string(), "uncertain".into()),
                (final_receipt.row_version.to_string(), "failed".into())
            ]
        );
        let queue_state: (i64, i64) = store
            .conn
            .query_row(
                "SELECT count(*),sum(reconciled_at IS NOT NULL)
                 FROM harness_manager_action_notice_queue WHERE operation_id=?1",
                [final_receipt.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(queue_state, (2, 2));
    }

    #[test]
    fn action_notice_construction_rolls_back_crash_boundaries_and_recovers_after_restart() {
        for fault in [
            ManagerActionNoticeFault::AfterWatch,
            ManagerActionNoticeFault::AfterNotice,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join(format!("action-notice-{fault:?}.db"));
            let store = Store::open(&database).unwrap();
            let (config, lead) = fixture(
                &store,
                ManagerPolicyV2 {
                    mode: ManagerOperatingModeV2::Execute,
                    capabilities: vec![ManagerCapabilityV2::LeadControl],
                    ..Default::default()
                },
            );
            let policy_version = store
                .get_harness_manager_policy(config.project_id)
                .unwrap()
                .unwrap()
                .row_version;
            let queued = store
                .enqueue_manager_action(
                    crate::store::manager_actions::ManagerActionOriginV2::Agent {
                        caller: config.manager_session_id,
                    },
                    AgentManagerControlRequestV2 {
                        fence: ManagerFenceV2 {
                            scope_version: config.row_version,
                            policy_version,
                        },
                        idempotency_key: format!("action-notice-{fault:?}"),
                        operation: ManagerActionV2::PauseLead {
                            epic_id: lead.parent_id.unwrap(),
                            expected: store
                                .manager_action_lead_fence(lead.parent_id.unwrap())
                                .unwrap(),
                            reason: "exercise atomic action notice construction".into(),
                        },
                    },
                )
                .unwrap();
            let claim = store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap();
            let receipt = store
                .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "test_terminal")
                .unwrap();
            let (job_id, _, _) = store.manager_action_watch_identity(&config);
            manager_action_notice_fail_next(fault);
            assert!(
                store
                    .reconcile_manager_action_notice(queued.operation_id)
                    .unwrap_err()
                    .to_string()
                    .contains("injected manager action notice fault")
            );
            let rolled_back: (i64, i64, i64, i64) = store
                .conn
                .query_row(
                    "SELECT
                       (SELECT count(*) FROM scheduled_jobs WHERE id=?1),
                       (SELECT count(*) FROM harness_manager_watches WHERE job_id=?1),
                       (SELECT count(*) FROM harness_manager_notices
                          WHERE kind='action_result' AND subject_id=?2),
                       (SELECT count(*) FROM harness_manager_action_notice_queue
                          WHERE operation_id=?2 AND operation_row_version=?3
                            AND reconciled_at IS NULL AND retired_at IS NULL)",
                    params![
                        job_id.to_string(),
                        receipt.operation_id.to_string(),
                        receipt.row_version
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(rolled_back, (0, 0, 0, 1));
            drop(store);

            let reopened = Store::open(&database).unwrap();
            assert_eq!(
                reopened
                    .reconcile_manager_action_notice(queued.operation_id)
                    .unwrap(),
                Some(job_id)
            );
            let recovered: (i64, i64, i64, String) = reopened
                .conn
                .query_row(
                    "SELECT
                       (SELECT count(*) FROM scheduled_jobs WHERE id=?1),
                       (SELECT count(*) FROM harness_manager_watches
                          WHERE job_id=?1 AND route_kind='manager_action'),
                       (SELECT count(*) FROM harness_manager_notices
                          WHERE job_id=?1 AND kind='action_result' AND subject_id=?2
                            AND subject_version=?3),
                       (SELECT message FROM scheduled_jobs WHERE id=?1)",
                    params![
                        job_id.to_string(),
                        receipt.operation_id.to_string(),
                        receipt.row_version.to_string()
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!((recovered.0, recovered.1, recovered.2), (1, 1, 1));
            assert!(recovered.3.contains(&receipt.operation_id.to_string()));
            assert!(
                recovered
                    .3
                    .contains(&format!("version={}", receipt.row_version))
            );
        }
    }

    #[test]
    fn action_notice_retry_adopts_an_exact_orphaned_scheduled_job() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let (job_id, _, _) = store.manager_action_watch_identity(&config);
        store
            .ensure_manager_action_watch(&config, "orphan-before-binding")
            .unwrap();
        store
            .conn
            .execute(
                "DELETE FROM harness_manager_watches WHERE job_id=?1",
                [job_id.to_string()],
            )
            .unwrap();
        let receipt = ManagerActionReceiptV2 {
            operation_id: Uuid::new_v4(),
            state: ManagerActionStateV2::Succeeded,
            action_kind: ManagerActionKindV2::PauseLead,
            target_type: ManagerActionTargetTypeV2::EpicLead,
            row_version: 3,
            target_session_id: Some(lead.id),
            outcome: Some("lead_committed".into()),
            result: Some(ManagerActionResultV2::LeadPaused),
            deduplicated: false,
            operator_result: None,
        };
        let operation = action_operation(
            &config,
            ManagerActionV2::PauseLead {
                epic_id: lead.parent_id.unwrap(),
                expected: store
                    .manager_action_lead_fence(lead.parent_id.unwrap())
                    .unwrap(),
                reason: "recover the exact orphaned transport".into(),
            },
            Some(lead.id),
            receipt.clone(),
        );
        assert_eq!(
            store
                .record_manager_action_notice(&config, &operation)
                .unwrap(),
            Some(job_id)
        );
        let recovered: (i64, i64, String) = store
            .conn
            .query_row(
                "SELECT
                   (SELECT count(*) FROM scheduled_jobs WHERE id=?1),
                   (SELECT count(*) FROM harness_manager_watches
                      WHERE job_id=?1 AND route_kind='manager_action'),
                   (SELECT message FROM scheduled_jobs WHERE id=?1)",
                [job_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((recovered.0, recovered.1), (1, 1));
        assert!(recovered.2.contains(&receipt.operation_id.to_string()));
    }

    #[test]
    fn action_reconciliation_recovers_an_old_missing_receipt_beyond_latest_window() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(
            &store,
            ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![ManagerCapabilityV2::LeadControl],
                ..Default::default()
            },
        );
        let policy_version = store
            .get_harness_manager_policy(config.project_id)
            .unwrap()
            .unwrap()
            .row_version;
        let expected = store
            .manager_action_lead_fence(lead.parent_id.unwrap())
            .unwrap();
        let mut receipts = Vec::new();
        for index in 0..=MAX_NOTICE_RECONCILIATION_BATCH {
            let queued = store
                .enqueue_manager_action(
                    crate::store::manager_actions::ManagerActionOriginV2::Agent {
                        caller: config.manager_session_id,
                    },
                    AgentManagerControlRequestV2 {
                        fence: ManagerFenceV2 {
                            scope_version: config.row_version,
                            policy_version,
                        },
                        idempotency_key: format!("terminal-action-{index}"),
                        operation: ManagerActionV2::PauseLead {
                            epic_id: lead.parent_id.unwrap(),
                            expected: expected.clone(),
                            reason: "exercise terminal notice reconciliation".into(),
                        },
                    },
                )
                .unwrap();
            let claim = store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap();
            assert_eq!(claim.id(), queued.operation_id);
            receipts.push(
                store
                    .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "test_terminal")
                    .unwrap(),
            );
        }
        store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations
                 SET updated_at='2000-01-01T00:00:00.000000000Z' WHERE id=?1",
                [receipts[0].operation_id.to_string()],
            )
            .unwrap();
        for receipt in &receipts[1..] {
            let operation = store
                .manager_action_operation(receipt.operation_id)
                .unwrap()
                .unwrap();
            store
                .record_manager_action_notice(&config, &operation)
                .unwrap();
        }
        let oldest_before: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='action_result' AND subject_id=?1",
                [receipts[0].operation_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(oldest_before, 0);

        store.manager_v2_reconcile_notices(&config).unwrap();

        let oldest_after: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='action_result' AND subject_id=?1
                   AND subject_version=?2",
                params![
                    receipts[0].operation_id.to_string(),
                    receipts[0].row_version.to_string()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(oldest_after, 1);
        let action_notices: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices WHERE kind='action_result'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(action_notices, MAX_NOTICE_RECONCILIATION_BATCH + 1);
    }

    #[test]
    fn retrieving_message_notice_preserves_the_genuine_pending_request() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let receipt = store
            .manager_send(
                config.manager_session_id,
                &rsi_common::harness_manager::AgentManagerSendRequestV1 {
                    epic_id: lead.parent_id.unwrap(),
                    message: "Report the focused verification evidence".into(),
                    idempotency_key: "pending-mail".into(),
                },
            )
            .unwrap();
        let inbox = store.manager_inbox(lead.id, &Default::default()).unwrap();
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.messages[0].message_id, receipt.message_id);
        let notice = inbox
            .notices
            .iter()
            .find(|notice| notice.kind == "message")
            .unwrap();
        assert_eq!(notice.subject_id, receipt.message_id.to_string());
        assert_eq!(notice.state["message_sequence"], receipt.sequence);
        assert!(
            !store.manager_request_replied(receipt.message_id).unwrap(),
            "notice retrieval is not a reply"
        );
        assert_eq!(
            store
                .manager_progress_page(
                    config.manager_session_id,
                    &rsi_common::harness_manager::AgentManagerProgressRequestV1 {
                        after_epic_id: None,
                        limit: None,
                    },
                )
                .unwrap()
                .recent_requests[0]
                .state,
            "pending_reply"
        );
    }

    #[test]
    fn request_filtered_notice_retrieval_uses_exact_message_exchange() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let request = store
            .manager_send(
                config.manager_session_id,
                &rsi_common::harness_manager::AgentManagerSendRequestV1 {
                    epic_id: lead.parent_id.unwrap(),
                    message: "Report the exact request evidence".into(),
                    idempotency_key: "filtered-request".into(),
                },
            )
            .unwrap();
        let lead_page = store
            .manager_inbox(
                lead.id,
                &rsi_common::harness_manager::AgentManagerInboxRequestV1 {
                    after_sequence: 0,
                    request_id: Some(request.message_id),
                    limit: 32,
                },
            )
            .unwrap();
        assert_eq!(lead_page.notices.len(), 1);
        assert_eq!(
            lead_page.notices[0].subject_id,
            request.message_id.to_string()
        );

        let reply = store
            .manager_reply(
                lead.id,
                &rsi_common::harness_manager::AgentManagerReplyRequestV1 {
                    request_id: request.message_id,
                    message: "Exact evidence attached".into(),
                    idempotency_key: "filtered-reply".into(),
                },
            )
            .unwrap();
        let manager_page = store
            .manager_inbox(
                config.manager_session_id,
                &rsi_common::harness_manager::AgentManagerInboxRequestV1 {
                    after_sequence: 0,
                    request_id: Some(request.message_id),
                    limit: 32,
                },
            )
            .unwrap();
        assert_eq!(manager_page.notices.len(), 1);
        assert_eq!(
            manager_page.notices[0].subject_id,
            reply.message_id.to_string()
        );
        assert_eq!(
            manager_page.notices[0].state["request_id"],
            request.message_id.to_string()
        );
    }

    #[test]
    fn failed_and_interrupted_session_versions_are_distinct_and_restart_safe() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("manager-notices.db");
        let store = Store::open(&database).unwrap();
        let (config, lead) = fixture(&store, policy());
        for status in [SessionStatus::Failed, SessionStatus::Interrupted] {
            store.update_session_status(lead.id, status).unwrap();
            store.reconcile_harness_manager_watches().unwrap();
        }
        let before: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='session_state' AND subject_id=?1",
                [lead.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 3);
        drop(store);

        let reopened = Store::open(&database).unwrap();
        reopened.reconcile_harness_manager_watches().unwrap();
        let after_restart_recovery: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='session_state' AND subject_id=?1",
                [lead.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            after_restart_recovery >= before,
            "restart may add an exact recovery transition but cannot erase history"
        );
        reopened.manager_v2_reconcile_notices(&config).unwrap();
        let after: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='session_state' AND subject_id=?1",
                [lead.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            after, after_restart_recovery,
            "both restart producers reuse the recovered exact version"
        );
    }

    #[test]
    fn oversized_question_projects_truthfully_without_rolling_back_other_project() {
        let store = Store::open_in_memory().unwrap();
        let (oversized_config, oversized_lead) = fixture(&store, policy());
        let (other_config, other_lead) = fixture(&store, policy());
        let question = PendingQuestion {
            questions: vec![QuestionItem {
                question: "q".repeat(70_000),
                header: "Oversized gate".into(),
                options: Vec::new(),
                multi_select: false,
            }],
        };
        store
            .update_session_pending_question_json(
                oversized_lead.id,
                Some(&serde_json::to_string(&question).unwrap()),
            )
            .unwrap();
        store
            .update_session_status(oversized_lead.id, SessionStatus::WaitingApproval)
            .unwrap();
        store
            .update_session_status(other_lead.id, SessionStatus::Failed)
            .unwrap();

        store.reconcile_harness_manager_watches().unwrap();

        let projection: String = store
            .conn
            .query_row(
                "SELECT json_extract(state_json,'$.question')
                 FROM harness_manager_notices
                 WHERE project_id=?1 AND scope_version=?2 AND kind='session_state'
                   AND subject_id=?3
                 ORDER BY sequence DESC LIMIT 1",
                params![
                    oversized_config.project_id.to_string(),
                    oversized_config.row_version,
                    oversized_lead.id.to_string()
                ],
                |row| row.get(0),
            )
            .unwrap();
        let projection: Value = serde_json::from_str(&projection).unwrap();
        assert_eq!(
            projection,
            json!({
                "state": "requires_session_view",
                "safe_error_class": "manager_question_requires_session_view",
                "question_count": 1,
                "serialized_bytes_at_least": MAX_NOTICE_QUESTION_BYTES + 1,
                "projection_limit_bytes": MAX_NOTICE_QUESTION_BYTES,
            })
        );
        assert!(serde_json::to_vec(&projection).unwrap().len() < 512);
        let other_recorded: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
                 WHERE project_id=?1 AND scope_version=?2 AND kind='session_state'
                   AND subject_id=?3 AND json_extract(state_json,'$.status')='Failed')",
                params![
                    other_config.project_id.to_string(),
                    other_config.row_version,
                    other_lead.id.to_string()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            other_recorded,
            "the second project's durable work commits independently"
        );
    }

    #[test]
    fn v2_ledger_notice_materialization_advances_in_fixed_cursor_pages() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        for index in 0..(MAX_NOTICE_RECORDS_PER_EPIC_PASS * 2 + 3) {
            store
                .manager_v2_record_changed(
                    &config,
                    "intent",
                    &format!("bulk-{index:03}"),
                    lead.parent_id,
                    &json!({"state":"active","index":index}),
                )
                .unwrap();
        }
        let expected = MAX_NOTICE_RECORDS_PER_EPIC_PASS * 2 + 3;
        let mut previous = 0;
        for _ in 0..4 {
            store.manager_v2_reconcile_notices(&config).unwrap();
            let count: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notices
                     WHERE project_id=?1 AND scope_version=?2 AND kind='ledger_change'
                       AND subject_id LIKE 'intent:bulk-%'",
                    params![config.project_id.to_string(), config.row_version],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(count - previous <= MAX_NOTICE_RECORDS_PER_EPIC_PASS);
            previous = count;
            if count == expected {
                break;
            }
        }
        assert_eq!(previous, expected);
        let cursor_rows: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notice_reconcile_cursors
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND lane='v2_intent' AND owner_id=?4",
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    lead.parent_id.unwrap().to_string()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor_rows, 1);
    }

    #[test]
    fn scope_replacement_retires_old_notices_in_restart_safe_bounded_pages() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("manager-scope-retirement.db");
        let store = Store::open(&database).unwrap();
        let (config, lead) = fixture(&store, policy());
        let settled = now();
        store
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retrieved_at=?4,settled_at=?4
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND retired_at IS NULL AND settled_at IS NULL",
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    settled
                ],
            )
            .unwrap();
        for index in 0..=MAX_NOTICE_RECONCILIATION_BATCH {
            insert_pending_test_notice(
                &store,
                &config,
                &lead,
                Uuid::new_v4(),
                &format!("old-scope-{index:04}"),
            );
        }

        let revoked = store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: config.project_id,
                    session_id: config.manager_session_id,
                    epic_ids: Some(Vec::new()),
                    group_ids: Vec::new(),
                    expected_row_version: config.row_version,
                },
            )
            .unwrap();
        let first_page: (i64, i64) = store
            .conn
            .query_row(
                "SELECT sum(retired_at IS NOT NULL),sum(retired_at IS NULL)
                 FROM harness_manager_notices WHERE subject_id LIKE 'old-scope-%'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(first_page, (MAX_NOTICE_RECONCILIATION_BATCH, 1));
        let pending_owner: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notice_scope_retirements
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND completed_at IS NULL",
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending_owner, 1);
        drop(store);

        let reopened = Store::open(&database).unwrap();
        reopened.reconcile_harness_manager_watches().unwrap();
        let retired_without_retrieval: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'old-scope-%' AND retired_at IS NOT NULL
                   AND retrieved_at IS NULL AND settled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retired_without_retrieval,
            MAX_NOTICE_RECONCILIATION_BATCH + 1
        );
        let completed_owner: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notice_scope_retirements
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND completed_at IS NOT NULL",
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(completed_owner, 1);

        let regranted = reopened
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: config.project_id,
                    session_id: config.manager_session_id,
                    epic_ids: Some(vec![lead.parent_id.unwrap()]),
                    group_ids: Vec::new(),
                    expected_row_version: revoked.row_version,
                },
            )
            .unwrap();
        assert!(regranted.epic_ids.contains(&lead.parent_id.unwrap()));
        let old_live: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE subject_id LIKE 'old-scope-%' AND retired_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_live, 0);
    }

    #[test]
    fn scope_edit_retires_its_exact_owner_without_draining_older_owners() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let settled = now();
        store
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retrieved_at=?4,settled_at=?4
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND retired_at IS NULL AND settled_at IS NULL",
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    settled
                ],
            )
            .unwrap();
        for index in 0..=MAX_NOTICE_RECONCILIATION_BATCH {
            insert_pending_test_notice(
                &store,
                &config,
                &lead,
                Uuid::new_v4(),
                &format!("exact-owner-{index:04}"),
            );
        }
        for index in 0..MAX_SCOPE_RETIREMENTS_PER_PASS {
            store
                .conn
                .execute(
                    "INSERT INTO harness_manager_notice_scope_retirements(
                        project_id,manager_session_id,scope_version,retired_at)
                     VALUES(?1,?2,1,?3)",
                    params![
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        format!("2000-01-01T00:00:{index:02}.000000000Z")
                    ],
                )
                .unwrap();
        }

        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: config.project_id,
                    session_id: config.manager_session_id,
                    epic_ids: Some(Vec::new()),
                    group_ids: Vec::new(),
                    expected_row_version: config.row_version,
                },
            )
            .unwrap();

        let exact_page: (i64, i64) = store
            .conn
            .query_row(
                "SELECT sum(retired_at IS NOT NULL),sum(retired_at IS NULL)
                 FROM harness_manager_notices WHERE subject_id LIKE 'exact-owner-%'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(exact_page, (MAX_NOTICE_RECONCILIATION_BATCH, 1));
        let older_mutations: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notice_scope_retirements
                 WHERE retired_at LIKE '2000-%' AND completed_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(older_mutations, 0);
    }

    #[test]
    fn failed_action_notice_then_scope_change_retires_exact_queue_obligation() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("action-notice-scope-retirement.db");
        let store = Store::open(&database).unwrap();
        let (config, lead) = fixture(
            &store,
            ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![ManagerCapabilityV2::LeadControl],
                ..Default::default()
            },
        );
        let policy_version = store
            .get_harness_manager_policy(config.project_id)
            .unwrap()
            .unwrap()
            .row_version;
        let queued = store
            .enqueue_manager_action(
                crate::store::manager_actions::ManagerActionOriginV2::Agent {
                    caller: config.manager_session_id,
                },
                AgentManagerControlRequestV2 {
                    fence: ManagerFenceV2 {
                        scope_version: config.row_version,
                        policy_version,
                    },
                    idempotency_key: "failed-notice-before-scope-change".into(),
                    operation: ManagerActionV2::PauseLead {
                        epic_id: lead.parent_id.unwrap(),
                        expected: store
                            .manager_action_lead_fence(lead.parent_id.unwrap())
                            .unwrap(),
                        reason: "retain the exact old-scope receipt".into(),
                    },
                },
            )
            .unwrap();
        let claim = store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap();
        let receipt = store
            .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "test_terminal")
            .unwrap();
        manager_action_notice_fail_next(ManagerActionNoticeFault::AfterWatch);
        assert!(
            store
                .reconcile_manager_action_notice(queued.operation_id)
                .is_err()
        );
        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: config.project_id,
                    session_id: config.manager_session_id,
                    epic_ids: Some(Vec::new()),
                    group_ids: Vec::new(),
                    expected_row_version: config.row_version,
                },
            )
            .unwrap();
        let retired: (i64, i64, i64) = store
            .conn
            .query_row(
                "SELECT reconciled_at IS NULL,retired_at IS NOT NULL,receipt_json=?3
                 FROM harness_manager_action_notice_queue
                 WHERE operation_id=?1 AND operation_row_version=?2",
                params![
                    receipt.operation_id.to_string(),
                    receipt.row_version,
                    serde_json::to_string(&receipt).unwrap()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(retired, (1, 1, 1));
        drop(store);

        let reopened = Store::open(&database).unwrap();
        reopened.reconcile_harness_manager_watches().unwrap();
        let still_retired: (i64, i64) = reopened
            .conn
            .query_row(
                "SELECT reconciled_at IS NULL,retired_at IS NOT NULL
                 FROM harness_manager_action_notice_queue
                 WHERE operation_id=?1 AND operation_row_version=?2",
                params![receipt.operation_id.to_string(), receipt.row_version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(still_retired, (1, 1));
        let materialized: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='action_result' AND subject_id=?1 AND subject_version=?2",
                params![
                    receipt.operation_id.to_string(),
                    receipt.row_version.to_string()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(materialized, 0);
    }

    #[test]
    fn action_finishing_after_scope_change_is_immediately_retired() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(
            &store,
            ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![ManagerCapabilityV2::LeadControl],
                ..Default::default()
            },
        );
        let policy_version = store
            .get_harness_manager_policy(config.project_id)
            .unwrap()
            .unwrap()
            .row_version;
        let queued = store
            .enqueue_manager_action(
                crate::store::manager_actions::ManagerActionOriginV2::Agent {
                    caller: config.manager_session_id,
                },
                AgentManagerControlRequestV2 {
                    fence: ManagerFenceV2 {
                        scope_version: config.row_version,
                        policy_version,
                    },
                    idempotency_key: "terminal-after-scope-change".into(),
                    operation: ManagerActionV2::PauseLead {
                        epic_id: lead.parent_id.unwrap(),
                        expected: store
                            .manager_action_lead_fence(lead.parent_id.unwrap())
                            .unwrap(),
                        reason: "finish after the old owner drains".into(),
                    },
                },
            )
            .unwrap();
        let claim = store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap();
        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: config.project_id,
                    session_id: config.manager_session_id,
                    epic_ids: Some(Vec::new()),
                    group_ids: Vec::new(),
                    expected_row_version: config.row_version,
                },
            )
            .unwrap();
        let receipt = store
            .finish_manager_action(&claim, ManagerActionStateV2::Revoked, "scope_changed")
            .unwrap();
        assert_eq!(receipt.operation_id, queued.operation_id);
        let state: (i64, i64) = store
            .conn
            .query_row(
                "SELECT reconciled_at IS NULL,retired_at IS NOT NULL
                 FROM harness_manager_action_notice_queue
                 WHERE operation_id=?1 AND operation_row_version=?2",
                params![receipt.operation_id.to_string(), receipt.row_version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, (1, 1));
    }

    #[test]
    fn distinct_subject_burst_is_transport_bounded_without_erasing_durable_versions() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let baseline: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='session_state' AND subject_id=?1",
                [lead.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        for index in 1..=300 {
            let mut transition = lead.clone();
            transition.status = if index % 2 == 0 {
                SessionStatus::Failed
            } else {
                SessionStatus::Interrupted
            };
            transition.updated_at += chrono::Duration::nanoseconds(index);
            store
                .record_manager_session_notice(&config, &transition)
                .unwrap();
        }
        let retained: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='session_state' AND subject_id=?1",
                [lead.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, baseline + 300);

        let job_id: String = store
            .conn
            .query_row(
                "SELECT job_id FROM harness_manager_notices
                 WHERE kind='session_state' AND subject_id=?1 LIMIT 1",
                [lead.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let job = store
            .get_scheduled_job(&Uuid::parse_str(&job_id).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            job.message
                .lines()
                .filter(|line| line.starts_with("- session_state "))
                .count(),
            MAX_RENDERED_NOTICES as usize
        );
        assert!(job.message.contains("additional notices remain"));
    }

    #[test]
    fn manager_v2_question_projection_resolves_legacy_blocker_after_new_publication() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        raise_question(&store, lead.id, 1);
        store
            .manager_v2_refresh_question_decisions(&config)
            .unwrap();
        let raw = serde_json::to_string(
            &store
                .get_session(lead.id)
                .unwrap()
                .unwrap()
                .pending_question
                .unwrap(),
        )
        .unwrap();
        store
            .update_session_pending_question_json(lead.id, Some(&raw))
            .unwrap();
        store
            .manager_v2_refresh_question_decisions(&config)
            .unwrap();
        let key = format!("question:{}", lead.id);
        assert_eq!(
            store
                .manager_v2_record(&config, "decision", &key)
                .unwrap()
                .unwrap()
                .payload["status"],
            "target_unavailable"
        );
        assert_eq!(
            store
                .manager_v2_record(&config, "intent", &key)
                .unwrap()
                .unwrap()
                .payload["state"],
            "blocked"
        );
        raise_question(&store, lead.id, 2);
        store
            .manager_v2_refresh_question_decisions(&config)
            .unwrap();
        assert_eq!(
            store
                .manager_v2_record(&config, "decision", &key)
                .unwrap()
                .unwrap()
                .payload["status"],
            "pending"
        );
        assert_eq!(
            store
                .manager_v2_record(&config, "intent", &key)
                .unwrap()
                .unwrap()
                .payload["state"],
            "resolved"
        );
    }
    #[test]
    fn manager_v2_board_retains_unroutable_tool_approval_as_an_exact_visible_gate() {
        use rsi_common::types::{Approval, ApprovalStatus};
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let approval = Approval {
            id: Uuid::new_v4(),
            session_id: lead.id,
            tool_name: "Shell".into(),
            tool_input: json!({"command":"inspect source"}),
            status: ApprovalStatus::Pending,
            created_at: chrono::Utc::now(),
            resolved_at: None,
        };
        store.insert_approval(&approval).unwrap();
        let page = store
            .manager_v2_inspect_operator(
                config.project_id,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Decisions,
                    ..Default::default()
                },
            )
            .unwrap();
        let gate = page
            .rows
            .iter()
            .find(|r| r["approval_id"] == approval.id.to_string())
            .unwrap();
        assert_eq!(gate["session_id"], lead.id.to_string());
        assert_eq!(gate["gate_state"], "pending");
        assert_eq!(gate["route_state"], "unavailable");
        assert_eq!(
            store.get_pending_approvals(lead.id).unwrap()[0].status,
            ApprovalStatus::Pending
        );
    }
}
