//! Direct `SQLite` `ProgramRun` kernel.
//!
//! The schema fingerprint helpers live beside the row mappers and mutation
//! kernel so the V78 migration and reopen tests share one semantic catalog
//! definition.

// This focused internal kernel intentionally keeps the accepted crate-private
// symbol inventory and large atomic transaction helpers together. Integer
// inputs are range-checked at their wire/row boundaries before these helpers.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::enum_variant_names,
    clippy::format_push_string,
    clippy::map_unwrap_or,
    clippy::missing_const_for_fn,
    clippy::needless_pass_by_value,
    clippy::redundant_closure_for_method_calls,
    clippy::redundant_pub_crate,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

use crate::error::{DaemonError, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::program_runs::{
    AcknowledgeProgramRunWakeRequestV1, ClaimProgramRunActionRequestV1,
    CommitProgramRunOutputRequestV1, CreateProgramRunRequestV1,
    ProgramRunActionAcknowledgementResultV1, ProgramRunActionClaimResultV1, ProgramRunActionKindV1,
    ProgramRunActionPublicationResultV1, ProgramRunActionPurposeV1, ProgramRunActionStateV1,
    ProgramRunActionV1, ProgramRunActorKindV1, ProgramRunAttemptRefV1, ProgramRunAttemptStateV1,
    ProgramRunBudgetDimensionV1, ProgramRunBudgetV1, ProgramRunExternalReferenceResultV1,
    ProgramRunGateEvaluationV1, ProgramRunGateResultV1, ProgramRunLockDomainV1,
    ProgramRunLockStateV1, ProgramRunLockV1, ProgramRunMutationResultV1, ProgramRunNextActionV1,
    ProgramRunOperationV1, ProgramRunOperationalStatusV1, ProgramRunPageCursorV1, ProgramRunPageV1,
    ProgramRunReconciliationClassV1, ProgramRunReconciliationItemV1,
    ProgramRunReconciliationPageV1, ProgramRunRequiredGateStatusV1, ProgramRunStatusV1,
    ProgramRunTransitionPageV1, ProgramRunTransitionRequestV1, ProgramRunTransitionV1,
    ProgramRunV1, RecordProgramRunGateRequestV1, ResumeBlockedProgramRunRequestV1,
    canonical_program_run_json, deterministic_program_run_action_id,
    deterministic_program_run_attempt_id, deterministic_program_run_gate_id,
    deterministic_program_run_id, deterministic_program_run_lock_id,
    deterministic_program_run_transition_id, program_run_fingerprint,
    program_run_transition_allowed,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration as StdDuration, Instant};
use uuid::Uuid;

use super::Store;

type ProgramRunStoreResult<T> = std::result::Result<T, ProgramRunStoreError>;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum D05SemanticFault {
    AfterMutationFacts,
    AfterBudget,
    AfterRunProjection,
    AfterIdeaProjection,
    AfterIdeaEvent,
    AfterTransition,
    AfterAction,
}

#[cfg(test)]
thread_local! {
    static D05_SEMANTIC_FAULT: std::cell::RefCell<Option<D05SemanticFault>> = const { std::cell::RefCell::new(None) };
    static D05_RECONCILE_DEADLINE_BEFORE_FIRST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn d05_test_fail_next_semantic(fault: D05SemanticFault) {
    D05_SEMANTIC_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn d05_test_force_reconcile_deadline_before_first() {
    D05_RECONCILE_DEADLINE_BEFORE_FIRST.with(|slot| slot.set(true));
}

#[cfg(test)]
fn d05_take_reconcile_deadline_before_first() -> bool {
    D05_RECONCILE_DEADLINE_BEFORE_FIRST.with(|slot| {
        let force = slot.get();
        slot.set(false);
        force
    })
}

#[cfg(test)]
fn d05_semantic_fault(fault: D05SemanticFault) -> ProgramRunStoreResult<()> {
    let hit = D05_SEMANTIC_FAULT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref() == Some(&fault) {
            slot.take();
            true
        } else {
            false
        }
    });
    if hit {
        Err(ProgramRunStoreError::StorageFailure)
    } else {
        Ok(())
    }
}

#[cfg(test)]
macro_rules! d05_fault {
    ($point:expr) => {
        d05_semantic_fault($point)?
    };
}

#[cfg(not(test))]
macro_rules! d05_fault {
    ($point:expr) => {{
        let _ = stringify!($point);
    }};
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Closed D05 error vocabulary includes boundary-only classifications.
pub(crate) enum ProgramRunStoreError {
    #[error("invalid request")]
    InvalidRequest,
    #[error("forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("an active run already exists")]
    ActiveRunExists,
    #[error("idempotency replay conflict")]
    ReplayConflict,
    #[error("stale run version")]
    StaleRunVersion,
    #[error("stale Idea version")]
    StaleIdeaVersion,
    #[error("stale controller epoch")]
    StaleControllerEpoch,
    #[error("controller mismatch")]
    ControllerMismatch,
    #[error("stale lease generation")]
    StaleLeaseGeneration,
    #[error("stale action claim generation")]
    StaleClaimGeneration,
    #[error("invalid ProgramRun transition")]
    InvalidTransition,
    #[error("cursor invariant violation")]
    CursorInvariant,
    #[error("required gate is incomplete")]
    GateIncomplete,
    #[error("ProgramRun budget exhausted")]
    BudgetExhausted,
    #[error("ProgramRun queue backpressure")]
    QueueBackpressure,
    #[error("ProgramRun lock unavailable")]
    LockUnavailable,
    #[error("terminal ProgramRun")]
    TerminalRun,
    #[error("ProgramRun is quarantined")]
    Quarantined,
    #[error("downstream replay conflict")]
    DownstreamReplayConflict,
    #[error("SQLite contention")]
    Contention,
    #[error("stored constraint violation")]
    ConstraintViolation,
    #[error("corrupt stored ProgramRun state")]
    CorruptStoredState,
    #[error("ProgramRun storage failure")]
    StorageFailure,
}

#[derive(Debug, Clone)]
pub(crate) struct ProgramRunStoreAuthority {
    actor_kind: ProgramRunActorKindV1,
    actor_session_id: Option<Uuid>,
    controller_session_id: Option<Uuid>,
    controller_epoch: Option<u64>,
    project_id: Option<Uuid>,
    idea_id: Option<Uuid>,
}

impl ProgramRunStoreAuthority {
    pub(crate) fn operator() -> Self {
        Self {
            actor_kind: ProgramRunActorKindV1::Operator,
            actor_session_id: None,
            controller_session_id: None,
            controller_epoch: None,
            project_id: None,
            idea_id: None,
        }
    }

    #[allow(dead_code)] // Constructed only behind the private D05 controller capability.
    fn controller(session_id: Uuid, epoch: u64) -> Self {
        Self {
            actor_kind: ProgramRunActorKindV1::Controller,
            actor_session_id: Some(session_id),
            controller_session_id: Some(session_id),
            controller_epoch: Some(epoch),
            project_id: None,
            idea_id: None,
        }
    }

    #[cfg(test)]
    fn scheduler(controller_session_id: Uuid, controller_epoch: u64) -> Self {
        Self::scheduler_scoped(controller_session_id, controller_epoch, None, None)
    }

    fn scheduler_scoped(
        controller_session_id: Uuid,
        controller_epoch: u64,
        project_id: Option<Uuid>,
        idea_id: Option<Uuid>,
    ) -> Self {
        Self {
            actor_kind: ProgramRunActorKindV1::Scheduler,
            actor_session_id: None,
            controller_session_id: Some(controller_session_id),
            controller_epoch: Some(controller_epoch),
            project_id,
            idea_id,
        }
    }
}

pub(crate) fn program_run_controller_authority_from_live_grant(
    grant: &crate::idea_control::BoundControllerWriteAuthority,
) -> ProgramRunStoreAuthority {
    ProgramRunStoreAuthority::controller(
        grant.controller_session_id(),
        u64::try_from(grant.controller_epoch()).unwrap_or_default(),
    )
}

pub(crate) fn program_run_scheduler_authority_from_live_grant(
    grant: &crate::idea_control::BoundControllerWriteAuthority,
) -> ProgramRunStoreAuthority {
    ProgramRunStoreAuthority::scheduler_scoped(
        grant.controller_session_id(),
        u64::try_from(grant.controller_epoch()).unwrap_or_default(),
        Some(grant.project_id()),
        Some(grant.idea_id()),
    )
}

#[cfg(test)]
pub(crate) fn test_program_run_controller_authority(
    session_id: Uuid,
    epoch: u64,
) -> ProgramRunStoreAuthority {
    ProgramRunStoreAuthority::controller(session_id, epoch)
}

#[cfg(test)]
pub(crate) fn test_program_run_scheduler_authority(
    session_id: Uuid,
    epoch: u64,
) -> ProgramRunStoreAuthority {
    ProgramRunStoreAuthority::scheduler(session_id, epoch)
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Controller-only variants are exercised through the private D05 authority seam.
pub(crate) enum ProgramRunTransitionInputV1 {
    Simple(ProgramRunTransitionRequestV1),
    ClaimAction(ClaimProgramRunActionRequestV1),
    AcknowledgeWake(AcknowledgeProgramRunWakeRequestV1),
    Resume(ResumeBlockedProgramRunRequestV1),
    CommitOutput(CommitProgramRunOutputRequestV1),
    Gate(RecordProgramRunGateRequestV1),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Model publication is an accepted capability seam, not a D05 provider adapter.
pub(crate) enum ProgramRunExternalReferenceV1 {
    ModelInvocation(Uuid),
    Session(Uuid),
    ScheduledJob(Uuid),
}

pub(crate) const PROGRAM_RUN_DISPATCH_VISIT_LIMIT: u32 = 128;
pub(crate) const PROGRAM_RUN_DISPATCH_EFFECT_LIMIT: u32 = 32;
const PROGRAM_RUN_DISPATCH_STATE_QUANTUM: u32 = 32;
const PROGRAM_RUN_DISPATCH_CURSOR_KEY: &str = "d05.program-run-dispatch.cursor.v1";
const PROGRAM_RUN_DISPATCH_STATES: [&str; 4] = ["reserved", "claimed", "published", "acknowledged"];

pub(crate) const PROGRAM_RUN_DISPATCH_EXACT_CANDIDATE_SQL: &str = "SELECT
       ((subject.controller_session_id=?2 AND subject.controller_epoch=?3
          AND (?4 IS NULL OR run.project_id=?4)
          AND (?5 IS NULL OR run.idea_id=?5)
          AND run.controller_session_id=subject.controller_session_id
          AND run.controller_epoch=subject.controller_epoch
          AND idea.project_id=run.project_id
          AND idea.current_controller_session_id=subject.controller_session_id
          AND idea.controller_epoch=subject.controller_epoch
          AND ((?7=1 AND subject.action_kind='work')
            OR (?8=1 AND subject.action_kind='wake')))
        AND ((subject.state='published' AND subject.claim_boot_id=?6
          AND (subject.external_model_invocation_id IS NOT NULL
            OR subject.external_session_id IS NOT NULL
            OR subject.scheduled_job_id IS NOT NULL))
        OR (subject.state='acknowledged' AND subject.claim_boot_id=?6
          AND subject.action_kind='wake' AND subject.purpose='retry_wake'
          AND subject.claim_run_version=run.row_version
          AND EXISTS(SELECT 1 FROM idea_program_run_transitions transition
            WHERE transition.id=subject.creating_transition_id
              AND transition.program_run_id=subject.program_run_id
              AND transition.resulting_run_version=run.row_version)
          AND run.status='retry_pending'
          AND NOT EXISTS(SELECT 1 FROM idea_program_run_actions active
            WHERE active.program_run_id=subject.program_run_id
              AND active.state IN ('reserved','claimed','published'))))) AS current,
       ((subject.controller_session_id=?2 AND subject.controller_epoch=?3
          AND (?4 IS NULL OR run.project_id=?4)
          AND (?5 IS NULL OR run.idea_id=?5)
          AND run.controller_session_id=subject.controller_session_id
          AND run.controller_epoch=subject.controller_epoch
          AND idea.project_id=run.project_id
          AND idea.current_controller_session_id=subject.controller_session_id
          AND idea.controller_epoch=subject.controller_epoch
          AND ((?7=1 AND subject.action_kind='work')
            OR (?8=1 AND subject.action_kind='wake')))
        AND ((subject.state='reserved' AND subject.not_before<=?9)
          OR (subject.state IN ('claimed','published')
            AND subject.claim_expires_at<=?9
            AND subject.external_model_invocation_id IS NULL
            AND subject.external_session_id IS NULL
            AND subject.scheduled_job_id IS NULL))) AS due
     FROM idea_program_run_actions AS subject
     JOIN idea_program_runs AS run ON run.id=subject.program_run_id
     JOIN ideas AS idea ON idea.id=run.idea_id
     WHERE subject.id=?1";

pub(crate) fn program_run_dispatch_scan_sql(cursor_predicate: &str) -> String {
    format!(
        "SELECT subject.id,subject.not_before,subject.controller_session_id,
            subject.state,subject.action_kind,subject.claim_boot_id,
            subject.claim_expires_at,
            (subject.external_model_invocation_id IS NOT NULL
              OR subject.external_session_id IS NOT NULL
              OR subject.scheduled_job_id IS NOT NULL) AS has_reference,
            (run.controller_session_id=subject.controller_session_id
              AND run.controller_epoch=subject.controller_epoch
              AND idea.project_id=run.project_id
              AND idea.current_controller_session_id=subject.controller_session_id
              AND idea.controller_epoch=subject.controller_epoch) AS current_scope,
            (subject.state='acknowledged'
              AND subject.action_kind='wake' AND subject.purpose='retry_wake'
              AND subject.claim_run_version=run.row_version
              AND EXISTS(SELECT 1 FROM idea_program_run_transitions transition
                WHERE transition.id=subject.creating_transition_id
                  AND transition.program_run_id=subject.program_run_id
                  AND transition.resulting_run_version=run.row_version)
              AND run.status='retry_pending'
              AND NOT EXISTS(SELECT 1 FROM idea_program_run_actions active
                WHERE active.program_run_id=subject.program_run_id
                  AND active.state IN ('reserved','claimed','published')))
              AS current_acknowledgement
     FROM idea_program_run_actions AS subject
       INDEXED BY idx_idea_program_run_actions_due
     JOIN idea_program_runs AS run ON run.id=subject.program_run_id
     JOIN ideas AS idea ON idea.id=run.idea_id
     WHERE subject.state=?1 AND ({cursor_predicate})
     ORDER BY subject.not_before,subject.id LIMIT ?4"
    )
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgramRunDispatchCursorV1 {
    version: u8,
    reserved: Option<ProgramRunDispatchPositionV1>,
    claimed: Option<ProgramRunDispatchPositionV1>,
    published: Option<ProgramRunDispatchPositionV1>,
    acknowledged: Option<ProgramRunDispatchPositionV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgramRunDispatchPositionV1 {
    not_before: String,
    action_id: Uuid,
}

impl Default for ProgramRunDispatchCursorV1 {
    fn default() -> Self {
        Self {
            version: 1,
            reserved: None,
            claimed: None,
            published: None,
            acknowledged: None,
        }
    }
}

impl ProgramRunDispatchCursorV1 {
    fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::default();
        };
        let Ok(cursor) = serde_json::from_str::<Self>(raw) else {
            return Self::default();
        };
        let valid_positions = [
            cursor.reserved.as_ref(),
            cursor.claimed.as_ref(),
            cursor.published.as_ref(),
            cursor.acknowledged.as_ref(),
        ]
        .into_iter()
        .flatten()
        .all(|position| !position.action_id.is_nil() && parse_time(&position.not_before).is_ok());
        if cursor.version == 1 && valid_positions {
            cursor
        } else {
            Self::default()
        }
    }

    fn position(&self, state: &str) -> Option<&ProgramRunDispatchPositionV1> {
        match state {
            "reserved" => self.reserved.as_ref(),
            "claimed" => self.claimed.as_ref(),
            "published" => self.published.as_ref(),
            "acknowledged" => self.acknowledged.as_ref(),
            _ => None,
        }
    }

    fn advance(&mut self, visit: &ProgramRunDispatchVisitV1) {
        let position = Some(ProgramRunDispatchPositionV1 {
            not_before: visit.not_before.clone(),
            action_id: visit.action_id,
        });
        match visit.state.as_str() {
            "reserved" => self.reserved = position,
            "claimed" => self.claimed = position,
            "published" => self.published = position,
            "acknowledged" => self.acknowledged = position,
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProgramRunDispatchVisitV1 {
    pub(crate) action_id: Uuid,
    pub(crate) controller_session_id: Uuid,
    pub(crate) dispatchable: bool,
    pub(crate) may_attempt_external_effect: bool,
    state: String,
    not_before: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ProgramRunDispatchBatchV1 {
    pub(crate) visits: Vec<ProgramRunDispatchVisitV1>,
    pub(crate) selection_queries: u32,
    observed_cursor: Option<String>,
    cursor: ProgramRunDispatchCursorV1,
}

pub(super) const D05_TABLES: [&str; 7] = [
    "idea_program_runs",
    "idea_program_run_transitions",
    "idea_program_run_gates",
    "idea_program_run_budgets",
    "idea_program_run_locks",
    "idea_program_run_actions",
    "idea_program_run_attempt_refs",
];

pub(super) const D05_TRIGGER_COUNT: i64 = 9;
pub(super) const D05_V78_SCHEMA_FINGERPRINT: &str =
    "sha256:e9947ede18ad2ded6e7a8d545b0d65db4b2f6c5612c3d39acfb04bb2b44db0f7";
pub(super) const D05_V79_SCHEMA_FINGERPRINT: &str =
    "sha256:9d7139598d76a773aec9ad9d781ca1c995722ec68a95b0e3cea96aef72b298a2";

pub(super) fn d05_schema_fingerprint(connection: &Connection) -> Result<String> {
    let mut material = String::new();
    let mut objects = connection.prepare(
        "SELECT type, name, tbl_name, COALESCE(sql, '')
         FROM sqlite_master
         WHERE tbl_name LIKE 'idea_program_run%'
         ORDER BY type, name",
    )?;
    let rows = objects.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (kind, name, table, sql) = row?;
        material.push_str(&format!(
            "object|{kind}|{name}|{table}|{}\n",
            normalize_catalog_sql(&sql)
        ));
    }

    for table in D05_TABLES {
        let mut columns = connection.prepare(&format!("PRAGMA table_xinfo('{table}')"))?;
        let rows = columns.query_map([], |row| {
            Ok(format!(
                "{}|{}|{}|{}|{}|{}|{}",
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        for row in rows {
            material.push_str(&format!("column|{table}|{}\n", row?));
        }

        let mut foreign_keys =
            connection.prepare(&format!("PRAGMA foreign_key_list('{table}')"))?;
        let rows = foreign_keys.query_map([], |row| {
            Ok(format!(
                "{}|{}|{}|{}|{}|{}|{}|{}",
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        for row in rows {
            material.push_str(&format!("foreign|{table}|{}\n", row?));
        }

        let mut indexes = connection.prepare(&format!("PRAGMA index_list('{table}')"))?;
        let indexes = indexes
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (sequence, name, unique, origin, partial) in indexes {
            material.push_str(&format!(
                "index|{table}|{sequence}|{name}|{unique}|{origin}|{partial}\n"
            ));
            let mut index_info = connection.prepare(&format!("PRAGMA index_xinfo('{name}')"))?;
            let rows = index_info.query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}|{}|{}|{}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?;
            for row in rows {
                material.push_str(&format!("index-column|{name}|{}\n", row?));
            }
        }
    }
    let digest = Sha256::digest(material.as_bytes());
    Ok(format!("sha256:{digest:x}"))
}

pub(super) fn d05_v78_schema_fingerprint(connection: &Connection) -> Result<String> {
    d05_schema_fingerprint(connection)
}

pub(super) fn validate_d05_v79_table_rows(connection: &Connection, table: &str) -> Result<()> {
    let (uuid_columns, time_columns): (&[&str], &[&str]) = match table {
        "idea_program_runs" => (
            &["id", "project_id", "idea_id", "controller_session_id"],
            &[
                "created_at",
                "updated_at",
                "settled_at",
                "cancelled_at",
                "failed_at",
            ],
        ),
        "idea_program_run_transitions" => (
            &["id", "program_run_id", "actor_session_id", "idea_event_id"],
            &["created_at"],
        ),
        "idea_program_run_gates" => (
            &[
                "id",
                "program_run_id",
                "transition_id",
                "evaluator_session_id",
            ],
            &["created_at"],
        ),
        "idea_program_run_budgets" => (&["program_run_id"], &["updated_at"]),
        "idea_program_run_locks" => (
            &[
                "id",
                "project_id",
                "program_run_id",
                "requesting_transition_id",
                "controller_session_id",
                "owner_boot_id",
            ],
            &[
                "requested_at",
                "acquired_at",
                "heartbeat_at",
                "expires_at",
                "released_at",
            ],
        ),
        "idea_program_run_actions" => (
            &[
                "id",
                "program_run_id",
                "creating_transition_id",
                "controller_session_id",
                "claim_boot_id",
                "external_model_invocation_id",
                "external_session_id",
                "scheduled_job_id",
            ],
            &[
                "not_before",
                "claimed_at",
                "claim_expires_at",
                "created_at",
                "updated_at",
                "published_at",
                "acknowledged_at",
            ],
        ),
        "idea_program_run_attempt_refs" => (
            &[
                "id",
                "program_run_id",
                "action_id",
                "session_id",
                "model_invocation_id",
                "creating_transition_id",
                "completing_transition_id",
            ],
            &["observed_at", "created_at", "updated_at"],
        ),
        _ => {
            return Err(DaemonError::Store(format!(
                "V79 validator received unknown table {table}"
            )));
        }
    };
    for column in uuid_columns {
        let mut statement = connection.prepare(&format!(
            "SELECT {column} FROM {table} WHERE {column} IS NOT NULL"
        ))?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        for value in values {
            let value = value?;
            let parsed = Uuid::parse_str(&value).map_err(|_| {
                DaemonError::Store(format!("V79 rejects noncanonical UUID in {table}.{column}"))
            })?;
            if parsed.is_nil() || parsed.to_string() != value {
                return Err(DaemonError::Store(format!(
                    "V79 rejects noncanonical UUID in {table}.{column}"
                )));
            }
        }
    }
    for column in time_columns {
        let mut statement = connection.prepare(&format!(
            "SELECT {column} FROM {table} WHERE {column} IS NOT NULL"
        ))?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        for value in values {
            let value = value?;
            let parsed = chrono::DateTime::parse_from_rfc3339(&value).map_err(|_| {
                DaemonError::Store(format!(
                    "V79 rejects malformed timestamp in {table}.{column}"
                ))
            })?;
            if parsed.offset().local_minus_utc() != 0
                || parsed
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Nanos, true)
                    != value
            {
                return Err(DaemonError::Store(format!(
                    "V79 rejects malformed timestamp in {table}.{column}"
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_d05_catalog(connection: &Connection) -> Result<()> {
    let tables: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master
         WHERE type = 'table' AND name IN (
            'idea_program_runs','idea_program_run_transitions','idea_program_run_gates',
            'idea_program_run_budgets','idea_program_run_locks','idea_program_run_actions',
            'idea_program_run_attempt_refs')",
        [],
        |row| row.get(0),
    )?;
    let triggers: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master
         WHERE type = 'trigger' AND tbl_name LIKE 'idea_program_run%'",
        [],
        |row| row.get(0),
    )?;
    if tables != D05_TABLES.len() as i64 || triggers != D05_TRIGGER_COUNT {
        return Err(DaemonError::Store(format!(
            "V78 ProgramRun catalog mismatch: tables={tables}, triggers={triggers}"
        )));
    }
    let fingerprint = d05_v78_schema_fingerprint(connection)?;
    if fingerprint != D05_V78_SCHEMA_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V78 ProgramRun semantic fingerprint mismatch: {fingerprint}"
        )));
    }
    Ok(())
}

fn normalize_catalog_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "")
        .to_ascii_lowercase()
}

const RUN_COLUMNS: &str = "id,project_id,idea_id,template_key,template_version,template_digest,template_json,status,cursor_ordinal,cursor_key,cursor_phase,revision_no,controller_session_id,controller_epoch,idea_row_version,row_version,next_transition_sequence,created_at,updated_at,settled_at,cancelled_at,failed_at";

const TRANSITION_COLUMNS: &str = "id,program_run_id,sequence,operation,from_status,to_status,old_cursor_ordinal,new_cursor_ordinal,old_revision_no,new_revision_no,actor_kind,actor_session_id,controller_epoch,expected_run_version,resulting_run_version,expected_idea_version,resulting_idea_version,idea_event_id,idempotency_key,request_fingerprint,created_at";

impl Store {
    pub(crate) fn create_program_run_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        request: &CreateProgramRunRequestV1,
    ) -> ProgramRunStoreResult<ProgramRunMutationResultV1> {
        if authority.actor_kind != ProgramRunActorKindV1::Operator {
            return Err(ProgramRunStoreError::Forbidden);
        }
        let request = request
            .normalized()
            .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
        let canonical_request = canonical_program_run_json(&request)
            .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
        let request_fingerprint =
            program_run_fingerprint("program-run-create:v1", canonical_request.as_bytes());
        let run_id = deterministic_program_run_id(request.idea_id, &request.idempotency_key);
        let transition_id =
            deterministic_program_run_transition_id(run_id, &request.idempotency_key);
        let event_id = Uuid::new_v5(&transition_id, b"program-run-idea-event:v1");
        let now = Utc::now();
        let now_text = timestamp(now);

        let tx = immediate(&self.conn)?;
        if let Some((existing_id, existing_fingerprint)) = tx
            .query_row(
                "SELECT id,creation_request_fingerprint FROM idea_program_runs
                 WHERE creation_idempotency_key=?1",
                [&request.idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(map_sql)?
        {
            if existing_fingerprint != request_fingerprint || existing_id != run_id.to_string() {
                return Err(ProgramRunStoreError::ReplayConflict);
            }
            let transition = load_transition_by_id_tx(&tx, transition_id)?;
            let current_run = load_run_tx(&tx, run_id)?;
            let run = load_replayed_run_projection_tx(&tx, &current_run, &transition)?;
            tx.commit().map_err(map_sql)?;
            return Ok(ProgramRunMutationResultV1 {
                run,
                transition,
                idea_event_id: event_id,
                deduplicated: true,
            });
        }

        let idea = tx
            .query_row(
                "SELECT project_id,current_controller_session_id,controller_epoch,row_version,next_event_sequence
                 FROM ideas WHERE id=?1",
                [request.idea_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sql)?
            .ok_or(ProgramRunStoreError::NotFound)?;
        let project_id = parse_uuid(&idea.0)?;
        let controller_session_id = idea
            .1
            .as_deref()
            .map(parse_uuid)
            .transpose()?
            .ok_or(ProgramRunStoreError::ControllerMismatch)?;
        let controller_epoch = to_u64(idea.2)?;
        let idea_row_version = to_u64(idea.3)?;
        let idea_event_sequence = idea.4;
        if controller_epoch == 0 {
            return Err(ProgramRunStoreError::ControllerMismatch);
        }
        if idea_row_version != request.expected_idea_row_version {
            return Err(ProgramRunStoreError::StaleIdeaVersion);
        }
        let active_exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM idea_program_runs
                 WHERE idea_id=?1 AND status NOT IN ('settled','cancelled','failed'))",
                [request.idea_id.to_string()],
                |row| row.get(0),
            )
            .map_err(map_sql)?;
        if active_exists {
            return Err(ProgramRunStoreError::ActiveRunExists);
        }

        let template_json = request
            .template
            .canonical_json()
            .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
        let template_digest = request
            .template
            .digest()
            .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
        let first_cursor = request
            .template
            .cursors
            .first()
            .ok_or(ProgramRunStoreError::InvalidRequest)?;

        tx.execute(
            "INSERT INTO idea_program_runs (
                id,project_id,idea_id,template_key,template_version,template_digest,template_json,
                status,cursor_ordinal,cursor_key,cursor_phase,revision_no,controller_session_id,
                controller_epoch,idea_row_version,row_version,next_transition_sequence,
                creation_idempotency_key,creation_request_fingerprint,creation_request_json,
                created_at,updated_at,settled_at,cancelled_at,failed_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,'pending',0,?8,?9,0,?10,?11,?12,1,2,?13,?14,?15,?16,?16,NULL,NULL,NULL)",
            params![
                run_id.to_string(), project_id.to_string(), request.idea_id.to_string(),
                request.template.template_key, i64::from(request.template.template_version),
                template_digest, template_json, first_cursor.key, first_cursor.phase,
                controller_session_id.to_string(), to_i64(controller_epoch)?,
                to_i64(idea_row_version + 1)?, request.idempotency_key, request_fingerprint,
                canonical_request, now_text,
            ],
        )
        .map_err(map_insert_sql)?;

        let updated = tx
            .execute(
                "UPDATE ideas SET row_version=row_version+1,next_event_sequence=next_event_sequence+1,updated_at=?1
                 WHERE id=?2 AND row_version=?3",
                params![timestamp(now), request.idea_id.to_string(), to_i64(idea_row_version)?],
            )
            .map_err(map_sql)?;
        if updated != 1 {
            return Err(ProgramRunStoreError::StaleIdeaVersion);
        }

        let event_payload = canonical_program_run_json(&serde_json::json!({
            "operation": "create",
            "program_run_id": run_id,
            "transition_id": transition_id,
        }))
        .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
        tx.execute(
            "INSERT INTO idea_events (
                id,project_id,idea_id,sequence,event_type,actor_kind,actor_id,
                controller_session_id,controller_epoch,expected_row_version,resulting_row_version,
                idempotency_key,occurred_at,payload_json,artifact_digests_json,evidence_digests_json
             ) VALUES (?1,?2,?3,?4,'program_transitioned','operator','local-operator',NULL,NULL,?5,?6,?7,?8,?9,'[]','[]')",
            params![
                event_id.to_string(), project_id.to_string(), request.idea_id.to_string(),
                idea_event_sequence, to_i64(idea_row_version)?, to_i64(idea_row_version + 1)?,
                format!("program-run:{}", request.idempotency_key), timestamp(now), event_payload,
            ],
        )
        .map_err(map_sql)?;

        tx.execute(
            "INSERT INTO idea_program_run_transitions (
                id,program_run_id,sequence,operation,from_status,to_status,
                old_cursor_ordinal,old_cursor_key,old_cursor_phase,
                new_cursor_ordinal,new_cursor_key,new_cursor_phase,
                old_revision_no,new_revision_no,actor_kind,actor_session_id,controller_epoch,
                expected_run_version,resulting_run_version,expected_idea_version,resulting_idea_version,
                idea_event_id,idempotency_key,request_json,request_fingerprint,created_at
             ) VALUES (?1,?2,1,'create',NULL,'pending',NULL,NULL,NULL,0,?3,?4,0,0,'operator',NULL,?5,0,1,?6,?7,?8,?9,?10,?11,?12)",
            params![
                transition_id.to_string(), run_id.to_string(), first_cursor.key, first_cursor.phase,
                to_i64(controller_epoch)?, to_i64(idea_row_version)?, to_i64(idea_row_version + 1)?,
                event_id.to_string(), request.idempotency_key, canonical_request,
                request_fingerprint, timestamp(now),
            ],
        )
        .map_err(map_sql)?;

        for (dimension, limit) in request.template.budgets.as_pairs() {
            tx.execute(
                "INSERT INTO idea_program_run_budgets
                 (program_run_id,dimension,limit_value,reserved_value,used_value,row_version,updated_at)
                 VALUES (?1,?2,?3,0,0,0,?4)",
                params![run_id.to_string(), dimension.as_str(), to_i64(limit)?, timestamp(now)],
            )
            .map_err(map_sql)?;
        }

        let total_requested: i64 = tx
            .query_row(
                "SELECT count(*) FROM idea_program_run_locks WHERE project_id=?1 AND state='requested'",
                [project_id.to_string()],
                |row| row.get(0),
            )
            .map_err(map_sql)?;
        if total_requested + request.template.locks.len() as i64 > 1_024 {
            return Err(ProgramRunStoreError::QueueBackpressure);
        }
        for requirement in &request.template.locks {
            let lock_key = lock_key(project_id, request.idea_id, requirement.conflict_domain);
            let depth: i64 = tx
                .query_row(
                    "SELECT count(*) FROM idea_program_run_locks
                     WHERE project_id=?1 AND lock_key=?2 AND state='requested'",
                    params![project_id.to_string(), lock_key],
                    |row| row.get(0),
                )
                .map_err(map_sql)?;
            if depth >= 64 {
                return Err(ProgramRunStoreError::QueueBackpressure);
            }
            let lock_id = deterministic_program_run_lock_id(run_id, requirement.conflict_domain);
            tx.execute(
                "INSERT INTO idea_program_run_locks (
                    id,project_id,lock_key,conflict_domain,program_run_id,requesting_transition_id,
                    state,controller_session_id,controller_epoch,lease_generation,owner_boot_id,
                    requested_at,acquired_at,heartbeat_at,expires_at,released_at,release_reason,
                    idempotency_key,request_fingerprint
                 ) VALUES (?1,?2,?3,?4,?5,?6,'requested',?7,?8,1,NULL,?9,NULL,NULL,NULL,NULL,NULL,?10,?11)",
                params![
                    lock_id.to_string(), project_id.to_string(), lock_key,
                    requirement.conflict_domain.as_str(), run_id.to_string(), transition_id.to_string(),
                    controller_session_id.to_string(), to_i64(controller_epoch)?, timestamp(now),
                    format!("{}:{}", request.idempotency_key, requirement.conflict_domain.as_str()),
                    request_fingerprint,
                ],
            )
            .map_err(map_insert_sql)?;
        }

        let run = load_run_tx(&tx, run_id)?;
        let transition = load_transition_by_id_tx(&tx, transition_id)?;
        assert_active_action_cardinality_tx(&tx, run_id)?;
        tx.commit().map_err(map_sql)?;
        Ok(ProgramRunMutationResultV1 {
            run,
            transition,
            idea_event_id: event_id,
            deduplicated: false,
        })
    }

    pub(crate) fn get_program_run_v1(
        &self,
        run_id: Uuid,
    ) -> ProgramRunStoreResult<Option<ProgramRunV1>> {
        self.conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM idea_program_runs WHERE id=?1"),
                [run_id.to_string()],
                map_run_row,
            )
            .optional()
            .map_err(map_sql)?
            .map(TryInto::try_into)
            .transpose()
    }

    pub(crate) fn list_program_runs_v1(
        &self,
        project_id: Option<Uuid>,
        idea_id: Option<Uuid>,
        status: Option<ProgramRunStatusV1>,
        cursor: Option<&ProgramRunPageCursorV1>,
        limit: u32,
    ) -> ProgramRunStoreResult<ProgramRunPageV1> {
        if !(1..=256).contains(&limit) {
            return Err(ProgramRunStoreError::InvalidRequest);
        }
        let cursor_time = cursor.map(|cursor| timestamp(cursor.updated_at));
        let cursor_id = cursor.map(|cursor| cursor.id.to_string());
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM idea_program_runs
                 WHERE (?1 IS NULL OR project_id=?1)
                   AND (?2 IS NULL OR idea_id=?2)
                   AND (?3 IS NULL OR status=?3)
                   AND (?4 IS NULL OR updated_at>?4 OR (updated_at=?4 AND id>?5))
                 ORDER BY updated_at,id LIMIT ?6"
            ))
            .map_err(map_sql)?;
        let rows = statement
            .query_map(
                params![
                    project_id.map(|value| value.to_string()),
                    idea_id.map(|value| value.to_string()),
                    status.map(|value| value.as_str()),
                    cursor_time,
                    cursor_id,
                    i64::from(limit),
                ],
                map_run_row,
            )
            .map_err(map_sql)?;
        let mut items: Vec<ProgramRunV1> = Vec::new();
        for row in rows {
            items.push(row.map_err(map_sql)?.try_into()?);
        }
        let next_cursor = if items.len() == limit as usize {
            items.last().map(|last| ProgramRunPageCursorV1 {
                updated_at: last.updated_at,
                id: last.id,
            })
        } else {
            None
        };
        Ok(ProgramRunPageV1 { items, next_cursor })
    }

    pub(crate) fn list_program_run_transitions_v1(
        &self,
        run_id: Uuid,
        after_sequence: Option<u64>,
        limit: u32,
    ) -> ProgramRunStoreResult<ProgramRunTransitionPageV1> {
        if !(1..=200).contains(&limit) {
            return Err(ProgramRunStoreError::InvalidRequest);
        }
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {TRANSITION_COLUMNS} FROM idea_program_run_transitions
                 WHERE program_run_id=?1 AND (?2 IS NULL OR sequence>?2)
                 ORDER BY sequence LIMIT ?3"
            ))
            .map_err(map_sql)?;
        let rows = statement
            .query_map(
                params![
                    run_id.to_string(),
                    after_sequence.map(to_i64).transpose()?,
                    i64::from(limit)
                ],
                map_transition_row,
            )
            .map_err(map_sql)?;
        let mut items: Vec<ProgramRunTransitionV1> = Vec::new();
        for row in rows {
            items.push(row.map_err(map_sql)?.try_into()?);
        }
        let next_sequence = if items.len() == limit as usize {
            items.last().map(|item| item.sequence)
        } else {
            None
        };
        Ok(ProgramRunTransitionPageV1 {
            items,
            next_sequence,
        })
    }

    pub(crate) fn apply_program_run_transition_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        input: &ProgramRunTransitionInputV1,
    ) -> ProgramRunStoreResult<ProgramRunMutationResultV1> {
        let prepared = PreparedTransition::from_input(input)?;
        let tx = immediate(&self.conn)?;

        // Replay is intentionally resolved before any mutable witness. A byte-
        // equivalent retry remains successful after versions and epochs move.
        if let Some((transition_id, fingerprint)) = tx
            .query_row(
                "SELECT id,request_fingerprint FROM idea_program_run_transitions
                 WHERE program_run_id=?1 AND idempotency_key=?2",
                params![prepared.run_id.to_string(), prepared.idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(map_sql)?
        {
            if fingerprint != prepared.fingerprint {
                return Err(ProgramRunStoreError::ReplayConflict);
            }
            let transition = load_transition_by_id_tx(&tx, parse_uuid(&transition_id)?)?;
            let current_run = load_run_tx(&tx, prepared.run_id)?;
            let run = load_replayed_run_projection_tx(&tx, &current_run, &transition)?;
            let idea_event_id = transition.idea_event_id;
            tx.commit().map_err(map_sql)?;
            return Ok(ProgramRunMutationResultV1 {
                run,
                transition,
                idea_event_id,
                deduplicated: true,
            });
        }

        let run = load_run_tx(&tx, prepared.run_id)?;
        validate_transition_authority(authority, &run, prepared.operation)?;
        if run.status.is_terminal() {
            return Err(ProgramRunStoreError::TerminalRun);
        }
        if run.row_version != prepared.expected_run_version {
            return Err(ProgramRunStoreError::StaleRunVersion);
        }
        if run.idea_row_version != prepared.expected_idea_version {
            return Err(ProgramRunStoreError::StaleIdeaVersion);
        }
        let idea: (i64, i64, Option<String>) = tx
            .query_row(
                "SELECT row_version,next_event_sequence,current_controller_session_id
                 FROM ideas WHERE id=?1 AND project_id=?2",
                params![run.idea_id.to_string(), run.project_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(map_sql)?
            .ok_or(ProgramRunStoreError::NotFound)?;
        if to_u64(idea.0)? != prepared.expected_idea_version {
            return Err(ProgramRunStoreError::StaleIdeaVersion);
        }
        if idea.2.as_deref() != Some(run.controller_session_id.to_string().as_str()) {
            return Err(ProgramRunStoreError::ControllerMismatch);
        }

        let template: rsi_common::program_runs::ProgramRunTemplateV1 =
            serde_json::from_str(&run.template_json)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?;
        let template = template
            .normalized()
            .map_err(|_| ProgramRunStoreError::CorruptStoredState)?;
        let now = Utc::now();
        let transition_id =
            deterministic_program_run_transition_id(run.id, &prepared.idempotency_key);
        let event_id = Uuid::new_v5(&transition_id, b"program-run-idea-event:v1");
        let sequence = run.next_transition_sequence;
        let mut to_status = run.status;
        let mut cursor_ordinal = run.cursor_ordinal;
        let mut cursor_key = run.cursor_key.clone();
        let mut cursor_phase = run.cursor_phase.clone();
        let mut revision_no = run.revision_no;
        let mut new_action: Option<(
            ProgramRunActionKindV1,
            ProgramRunActionPurposeV1,
            DateTime<Utc>,
        )> = None;
        let mut productive = false;

        match input {
            ProgramRunTransitionInputV1::Simple(request) => match request.operation {
                ProgramRunOperationV1::LocksGranted => {
                    if run.status != ProgramRunStatusV1::Pending {
                        return Err(ProgramRunStoreError::InvalidTransition);
                    }
                    grant_all_locks_tx(&tx, &run, self.program_run_boot_id(), now)?;
                    to_status = ProgramRunStatusV1::Ready;
                    new_action = Some((
                        ProgramRunActionKindV1::Work,
                        ProgramRunActionPurposeV1::ExecuteCursor,
                        now,
                    ));
                    productive = true;
                }
                ProgramRunOperationV1::AttemptTerminalObserved => {
                    if run.status != ProgramRunStatusV1::Running {
                        return Err(ProgramRunStoreError::InvalidTransition);
                    }
                    let attempt = load_current_attempt_tx(&tx, &run)?
                        .ok_or(ProgramRunStoreError::ConstraintViolation)?;
                    if !matches!(
                        attempt.state,
                        ProgramRunAttemptStateV1::Launched
                            | ProgramRunAttemptStateV1::TerminalObserved
                    ) {
                        return Err(ProgramRunStoreError::InvalidTransition);
                    }
                    tx.execute(
                        "UPDATE idea_program_run_attempt_refs SET state='terminal_observed',
                         observed_session_status=?1,observed_at=?2,updated_at=?2 WHERE id=?3",
                        params![
                            request.reason.as_deref().unwrap_or("terminal"),
                            timestamp(now),
                            attempt.id.to_string()
                        ],
                    )
                    .map_err(map_sql)?;
                }
                ProgramRunOperationV1::RetryScheduled => {
                    if run.status != ProgramRunStatusV1::Running {
                        return Err(ProgramRunStoreError::InvalidTransition);
                    }
                    consume_budget_tx(
                        &tx,
                        run.id,
                        ProgramRunBudgetDimensionV1::LaunchRetries,
                        now,
                    )?;
                    consume_budget_tx(
                        &tx,
                        run.id,
                        ProgramRunBudgetDimensionV1::WakeReservations,
                        now,
                    )?;
                    finish_active_action_tx(
                        &tx,
                        run.id,
                        ProgramRunActionStateV1::Failed,
                        now,
                        request.reason.as_deref(),
                    )?;
                    to_status = ProgramRunStatusV1::RetryPending;
                    new_action = Some((
                        ProgramRunActionKindV1::Wake,
                        ProgramRunActionPurposeV1::RetryWake,
                        now,
                    ));
                    productive = true;
                }
                ProgramRunOperationV1::OperatorUnblocked => {
                    if run.status != ProgramRunStatusV1::Blocked {
                        return Err(ProgramRunStoreError::InvalidTransition);
                    }
                    to_status = ProgramRunStatusV1::Ready;
                    new_action = Some((
                        ProgramRunActionKindV1::Work,
                        ProgramRunActionPurposeV1::ExecuteCursor,
                        now,
                    ));
                    productive = true;
                }
                ProgramRunOperationV1::OperatorCancelled => {
                    to_status = ProgramRunStatusV1::Cancelled;
                    finish_active_action_tx(
                        &tx,
                        run.id,
                        ProgramRunActionStateV1::Cancelled,
                        now,
                        request.reason.as_deref(),
                    )?;
                    release_locks_tx(&tx, run.id, "operator_cancelled", now)?;
                }
                ProgramRunOperationV1::BudgetExhausted => {
                    to_status = ProgramRunStatusV1::Blocked;
                    finish_active_action_tx(
                        &tx,
                        run.id,
                        ProgramRunActionStateV1::Cancelled,
                        now,
                        request.reason.as_deref(),
                    )?;
                }
                ProgramRunOperationV1::ReconciledQuarantine => {
                    to_status = ProgramRunStatusV1::Blocked;
                    finish_active_action_tx(
                        &tx,
                        run.id,
                        ProgramRunActionStateV1::Failed,
                        now,
                        request.reason.as_deref(),
                    )?;
                    release_locks_tx(&tx, run.id, "reconciled_quarantine", now)?;
                }
                ProgramRunOperationV1::Create
                | ProgramRunOperationV1::ActionClaimed
                | ProgramRunOperationV1::WakeAcknowledged
                | ProgramRunOperationV1::AttemptOutputCommitted
                | ProgramRunOperationV1::GateEvaluated
                | ProgramRunOperationV1::ControllerRebound => {
                    return Err(ProgramRunStoreError::InvalidTransition);
                }
            },
            ProgramRunTransitionInputV1::ClaimAction(request) => {
                if run.status != ProgramRunStatusV1::Ready
                    || request.claim_boot_id != self.program_run_boot_id()
                {
                    return Err(ProgramRunStoreError::StaleClaimGeneration);
                }
                let action = validate_semantic_action_witness_tx(
                    &tx,
                    authority,
                    &run,
                    request.action_id,
                    request.claim_boot_id,
                    request.claim_generation,
                    request.claim_run_version,
                    request.claim_lease_generation,
                    ProgramRunActionKindV1::Work,
                    ProgramRunActionStateV1::Claimed,
                    now,
                )?;
                reserve_budget_tx(&tx, run.id, ProgramRunBudgetDimensionV1::WorkAttempts, now)?;
                let attempt_no = next_attempt_no_tx(&tx, &run)?;
                let attempt_id = deterministic_program_run_attempt_id(action.id, attempt_no);
                tx.execute(
                    "INSERT INTO idea_program_run_attempt_refs
                     (id,program_run_id,action_id,cursor_ordinal,cursor_key,revision_no,attempt_no,state,
                      session_id,model_invocation_id,observed_session_status,observed_at,output_ref,output_digest,
                      creating_transition_id,completing_transition_id,created_at,updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,'reserved',NULL,NULL,NULL,NULL,NULL,NULL,?8,NULL,?9,?9)",
                    params![
                        attempt_id.to_string(),
                        run.id.to_string(),
                        action.id.to_string(),
                        i64::from(
                            run.cursor_ordinal
                                .ok_or(ProgramRunStoreError::CursorInvariant)?
                        ),
                        run.cursor_key,
                        i64::from(run.revision_no),
                        i64::from(attempt_no),
                        transition_id.to_string(),
                        timestamp(now)
                    ],
                )
                .map_err(map_insert_sql)?;
                to_status = ProgramRunStatusV1::Running;
                productive = true;
            }
            ProgramRunTransitionInputV1::AcknowledgeWake(request) => {
                if run.status != ProgramRunStatusV1::RetryPending
                    || request.claim_boot_id != self.program_run_boot_id()
                {
                    return Err(ProgramRunStoreError::StaleClaimGeneration);
                }
                let action = validate_semantic_action_witness_tx(
                    &tx,
                    authority,
                    &run,
                    request.action_id,
                    request.claim_boot_id,
                    request.claim_generation,
                    request.claim_run_version,
                    request.claim_lease_generation,
                    ProgramRunActionKindV1::Wake,
                    ProgramRunActionStateV1::Acknowledged,
                    now,
                )?;
                consume_acknowledged_action_tx(
                    &tx,
                    &action,
                    request.claim_boot_id,
                    request.claim_generation,
                    request.claim_run_version,
                    request.claim_lease_generation,
                    now,
                )?;
                to_status = ProgramRunStatusV1::Ready;
                new_action = Some((
                    ProgramRunActionKindV1::Work,
                    ProgramRunActionPurposeV1::ExecuteCursor,
                    now,
                ));
                productive = true;
            }
            ProgramRunTransitionInputV1::Resume(_) => {
                if run.status != ProgramRunStatusV1::Blocked {
                    return Err(ProgramRunStoreError::InvalidTransition);
                }
                to_status = ProgramRunStatusV1::Ready;
                new_action = Some((
                    ProgramRunActionKindV1::Work,
                    ProgramRunActionPurposeV1::ExecuteCursor,
                    now,
                ));
                productive = true;
            }
            ProgramRunTransitionInputV1::CommitOutput(request) => {
                if run.status != ProgramRunStatusV1::Running {
                    return Err(ProgramRunStoreError::InvalidTransition);
                }
                let attempt = load_current_attempt_tx(&tx, &run)?
                    .ok_or(ProgramRunStoreError::ConstraintViolation)?;
                if attempt.id != request.attempt_id
                    || !matches!(
                        attempt.state,
                        ProgramRunAttemptStateV1::Launched
                            | ProgramRunAttemptStateV1::TerminalObserved
                    )
                    || !valid_digest(&request.output_digest)
                    || request.output_ref.is_empty()
                    || request.output_ref.len() > 2_048
                {
                    return Err(ProgramRunStoreError::InvalidRequest);
                }
                tx.execute(
                    "UPDATE idea_program_run_attempt_refs SET state='output_committed',output_ref=?1,
                     output_digest=?2,completing_transition_id=?3,updated_at=?4 WHERE id=?5",
                    params![request.output_ref, request.output_digest, transition_id.to_string(), timestamp(now), attempt.id.to_string()],
                ).map_err(map_sql)?;
                finish_active_action_tx(
                    &tx,
                    run.id,
                    ProgramRunActionStateV1::Acknowledged,
                    now,
                    None,
                )?;
                let ordinal =
                    run.cursor_ordinal
                        .ok_or(ProgramRunStoreError::CursorInvariant)? as usize;
                let cursor = template
                    .cursors
                    .get(ordinal)
                    .ok_or(ProgramRunStoreError::CursorInvariant)?;
                if cursor.required_gates.is_empty() {
                    if ordinal + 1 == template.cursors.len() {
                        to_status = ProgramRunStatusV1::Settled;
                        cursor_ordinal = None;
                        cursor_key = None;
                        cursor_phase = None;
                        release_locks_tx(&tx, run.id, "settled", now)?;
                    } else {
                        let next = &template.cursors[ordinal + 1];
                        to_status = ProgramRunStatusV1::Ready;
                        cursor_ordinal = Some((ordinal + 1) as u32);
                        cursor_key = Some(next.key.clone());
                        cursor_phase = Some(next.phase.clone());
                        new_action = Some((
                            ProgramRunActionKindV1::Work,
                            ProgramRunActionPurposeV1::ExecuteCursor,
                            now,
                        ));
                    }
                } else {
                    to_status = ProgramRunStatusV1::AwaitingGate;
                    new_action = Some((
                        ProgramRunActionKindV1::Work,
                        ProgramRunActionPurposeV1::EvaluateGates,
                        now,
                    ));
                }
                productive = true;
            }
            ProgramRunTransitionInputV1::Gate(request) => {
                if run.status != ProgramRunStatusV1::AwaitingGate
                    || request.gate_key.is_empty()
                    || !valid_digest(&request.evidence_digest)
                    || request.evidence_ref.is_empty()
                {
                    return Err(ProgramRunStoreError::InvalidTransition);
                }
                let ordinal =
                    run.cursor_ordinal
                        .ok_or(ProgramRunStoreError::CursorInvariant)? as usize;
                let cursor = template
                    .cursors
                    .get(ordinal)
                    .ok_or(ProgramRunStoreError::CursorInvariant)?;
                let requirement = cursor
                    .required_gates
                    .iter()
                    .find(|gate| gate.gate_key == request.gate_key)
                    .ok_or(ProgramRunStoreError::InvalidRequest)?;
                if requirement.policy_key != request.policy_key
                    || requirement.policy_version != request.policy_version
                {
                    return Err(ProgramRunStoreError::InvalidRequest);
                }
                let evaluation_no: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(evaluation_no),0)+1 FROM idea_program_run_gates
                     WHERE program_run_id=?1 AND cursor_ordinal=?2 AND revision_no=?3 AND gate_key=?4",
                    params![run.id.to_string(), ordinal as i64, i64::from(run.revision_no), request.gate_key],
                    |row| row.get(0),
                ).map_err(map_sql)?;
                let gate_id = deterministic_program_run_gate_id(run.id, &request.idempotency_key);
                tx.execute(
                    "INSERT INTO idea_program_run_gates
                     (id,program_run_id,cursor_ordinal,cursor_key,revision_no,gate_key,evaluation_no,result,
                      policy_key,policy_version,evidence_ref,evidence_digest,transition_id,idempotency_key,
                      request_fingerprint,request_json,evaluator_kind,evaluator_session_id,created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                    params![gate_id.to_string(), run.id.to_string(), ordinal as i64, cursor.key,
                        i64::from(run.revision_no), request.gate_key, evaluation_no, request.result.as_str(),
                        request.policy_key, i64::from(request.policy_version), request.evidence_ref,
                        request.evidence_digest, transition_id.to_string(), request.idempotency_key,
                        prepared.fingerprint, prepared.canonical_json, authority.actor_kind.as_str(),
                        authority.actor_session_id.map(|id| id.to_string()), timestamp(now)],
                ).map_err(map_insert_sql)?;
                match request.result {
                    ProgramRunGateResultV1::Blocked => {
                        to_status = ProgramRunStatusV1::Blocked;
                        finish_active_action_tx(
                            &tx,
                            run.id,
                            ProgramRunActionStateV1::Cancelled,
                            now,
                            Some("gate_blocked"),
                        )?;
                    }
                    ProgramRunGateResultV1::Failed => {
                        if let Some(target) = cursor.revision_target_ordinal {
                            consume_budget_tx(
                                &tx,
                                run.id,
                                ProgramRunBudgetDimensionV1::Revisions,
                                now,
                            )?;
                            consume_budget_tx(
                                &tx,
                                run.id,
                                ProgramRunBudgetDimensionV1::WakeReservations,
                                now,
                            )?;
                            finish_active_action_tx(
                                &tx,
                                run.id,
                                ProgramRunActionStateV1::Acknowledged,
                                now,
                                None,
                            )?;
                            revision_no = revision_no
                                .checked_add(1)
                                .ok_or(ProgramRunStoreError::CorruptStoredState)?;
                            let target_cursor = template
                                .cursors
                                .get(target as usize)
                                .ok_or(ProgramRunStoreError::CursorInvariant)?;
                            cursor_ordinal = Some(target);
                            cursor_key = Some(target_cursor.key.clone());
                            cursor_phase = Some(target_cursor.phase.clone());
                            to_status = ProgramRunStatusV1::RetryPending;
                            new_action = Some((
                                ProgramRunActionKindV1::Wake,
                                ProgramRunActionPurposeV1::RetryWake,
                                now,
                            ));
                        } else {
                            to_status = ProgramRunStatusV1::Blocked;
                            finish_active_action_tx(
                                &tx,
                                run.id,
                                ProgramRunActionStateV1::Cancelled,
                                now,
                                Some("gate_failed"),
                            )?;
                        }
                    }
                    ProgramRunGateResultV1::Passed => {
                        if all_required_gates_passed_tx(&tx, &run, cursor)? {
                            finish_active_action_tx(
                                &tx,
                                run.id,
                                ProgramRunActionStateV1::Acknowledged,
                                now,
                                None,
                            )?;
                            if ordinal + 1 == template.cursors.len() {
                                to_status = ProgramRunStatusV1::Settled;
                                cursor_ordinal = None;
                                cursor_key = None;
                                cursor_phase = None;
                                release_locks_tx(&tx, run.id, "settled", now)?;
                            } else {
                                let next = &template.cursors[ordinal + 1];
                                to_status = ProgramRunStatusV1::Ready;
                                cursor_ordinal = Some((ordinal + 1) as u32);
                                cursor_key = Some(next.key.clone());
                                cursor_phase = Some(next.phase.clone());
                                new_action = Some((
                                    ProgramRunActionKindV1::Work,
                                    ProgramRunActionPurposeV1::ExecuteCursor,
                                    now,
                                ));
                            }
                        }
                    }
                }
                productive = true;
            }
        }

        d05_fault!(D05SemanticFault::AfterMutationFacts);

        if !program_run_transition_allowed(run.status, to_status) {
            return Err(ProgramRunStoreError::InvalidTransition);
        }
        if productive {
            consume_budget_tx(
                &tx,
                run.id,
                ProgramRunBudgetDimensionV1::ProductiveTransitions,
                now,
            )?;
        }
        d05_fault!(D05SemanticFault::AfterBudget);

        let resulting_run_version = run.row_version + 1;
        let resulting_idea_version = run.idea_row_version + 1;
        let terminal_time = to_status.is_terminal().then(|| timestamp(now));
        let updated = tx.execute(
            "UPDATE idea_program_runs SET status=?1,cursor_ordinal=?2,cursor_key=?3,cursor_phase=?4,
             revision_no=?5,idea_row_version=?6,row_version=?7,next_transition_sequence=next_transition_sequence+1,
             updated_at=?8,settled_at=?9,cancelled_at=?10,failed_at=?11 WHERE id=?12 AND row_version=?13",
            params![to_status.as_str(), cursor_ordinal.map(i64::from), cursor_key, cursor_phase,
                i64::from(revision_no), to_i64(resulting_idea_version)?, to_i64(resulting_run_version)?,
                timestamp(now), if to_status == ProgramRunStatusV1::Settled { terminal_time.clone() } else { None },
                if to_status == ProgramRunStatusV1::Cancelled { terminal_time.clone() } else { None },
                if to_status == ProgramRunStatusV1::Failed { terminal_time } else { None },
                run.id.to_string(), to_i64(run.row_version)?],
        ).map_err(map_sql)?;
        if updated != 1 {
            return Err(ProgramRunStoreError::StaleRunVersion);
        }
        if let ProgramRunTransitionInputV1::ClaimAction(request) = input {
            let changed = tx
                .execute(
                    "UPDATE idea_program_run_actions SET claim_run_version=?1,updated_at=?2
                     WHERE id=?3 AND program_run_id=?4 AND state='claimed'
                       AND claim_boot_id=?5 AND claim_generation=?6
                       AND claim_run_version=?7 AND claim_lease_generation=?8",
                    params![
                        to_i64(resulting_run_version)?,
                        timestamp(now),
                        request.action_id.to_string(),
                        run.id.to_string(),
                        request.claim_boot_id.to_string(),
                        to_i64(request.claim_generation)?,
                        to_i64(request.claim_run_version)?,
                        to_i64(request.claim_lease_generation)?,
                    ],
                )
                .map_err(map_sql)?;
            if changed != 1 {
                return Err(ProgramRunStoreError::StaleClaimGeneration);
            }
        }
        d05_fault!(D05SemanticFault::AfterRunProjection);
        let updated = tx.execute(
            "UPDATE ideas SET row_version=row_version+1,next_event_sequence=next_event_sequence+1,updated_at=?1
             WHERE id=?2 AND row_version=?3",
            params![timestamp(now), run.idea_id.to_string(), to_i64(run.idea_row_version)?],
        ).map_err(map_sql)?;
        if updated != 1 {
            return Err(ProgramRunStoreError::StaleIdeaVersion);
        }
        d05_fault!(D05SemanticFault::AfterIdeaProjection);

        let event_type = if prepared.operation == ProgramRunOperationV1::GateEvaluated {
            "gate_transitioned"
        } else {
            "program_transitioned"
        };
        let payload = canonical_program_run_json(&serde_json::json!({
            "operation": prepared.operation.as_str(), "program_run_id": run.id,
            "transition_id": transition_id, "reason": prepared.reason,
        }))
        .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
        let idea_actor_kind = match authority.actor_kind {
            ProgramRunActorKindV1::Operator => "operator",
            ProgramRunActorKindV1::Controller | ProgramRunActorKindV1::Scheduler => "session",
            ProgramRunActorKindV1::System => "system",
        };
        tx.execute(
            "INSERT INTO idea_events
             (id,project_id,idea_id,sequence,event_type,actor_kind,actor_id,controller_session_id,
              controller_epoch,expected_row_version,resulting_row_version,idempotency_key,occurred_at,
              payload_json,artifact_digests_json,evidence_digests_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'[]','[]')",
            params![event_id.to_string(), run.project_id.to_string(), run.idea_id.to_string(), idea.1,
                event_type, idea_actor_kind, authority.actor_session_id.map(|id| id.to_string()).unwrap_or_else(|| "local-operator".into()),
                authority.controller_session_id.map(|id| id.to_string()), authority.controller_epoch.map(to_i64).transpose()?,
                to_i64(run.idea_row_version)?, to_i64(resulting_idea_version)?,
                format!("program-run:{}", prepared.idempotency_key), timestamp(now), payload],
        ).map_err(map_sql)?;
        d05_fault!(D05SemanticFault::AfterIdeaEvent);
        tx.execute(
            "INSERT INTO idea_program_run_transitions
             (id,program_run_id,sequence,operation,from_status,to_status,old_cursor_ordinal,old_cursor_key,
              old_cursor_phase,new_cursor_ordinal,new_cursor_key,new_cursor_phase,old_revision_no,new_revision_no,
              actor_kind,actor_session_id,controller_epoch,expected_run_version,resulting_run_version,
              expected_idea_version,resulting_idea_version,idea_event_id,idempotency_key,request_json,
              request_fingerprint,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26)",
            params![transition_id.to_string(), run.id.to_string(), to_i64(sequence)?, prepared.operation.as_str(),
                run.status.as_str(), to_status.as_str(), run.cursor_ordinal.map(i64::from), run.cursor_key,
                run.cursor_phase, cursor_ordinal.map(i64::from), cursor_key, cursor_phase,
                i64::from(run.revision_no), i64::from(revision_no), authority.actor_kind.as_str(),
                authority.actor_session_id.map(|id| id.to_string()), to_i64(run.controller_epoch)?,
                to_i64(run.row_version)?, to_i64(resulting_run_version)?, to_i64(run.idea_row_version)?,
                to_i64(resulting_idea_version)?, event_id.to_string(), prepared.idempotency_key,
                prepared.canonical_json, prepared.fingerprint, timestamp(now)],
        ).map_err(map_insert_sql)?;
        d05_fault!(D05SemanticFault::AfterTransition);
        if let Some((kind, purpose, not_before)) = new_action {
            let committed_projection = load_run_tx(&tx, run.id)?;
            insert_action_tx(
                &tx,
                &committed_projection,
                transition_id,
                kind,
                purpose,
                not_before,
                &template,
                now,
            )?;
        }
        d05_fault!(D05SemanticFault::AfterAction);
        assert_active_action_cardinality_tx(&tx, run.id)?;
        let committed_run = load_run_tx(&tx, run.id)?;
        let transition = load_transition_by_id_tx(&tx, transition_id)?;
        tx.commit().map_err(map_sql)?;
        Ok(ProgramRunMutationResultV1 {
            run: committed_run,
            transition,
            idea_event_id: event_id,
            deduplicated: false,
        })
    }

    #[allow(dead_code)] // Production dispatch uses exact candidates; Store falsifiers use pages.
    pub(crate) fn claim_due_program_run_actions_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        boot_id: Uuid,
        supported_kinds: &[ProgramRunActionKindV1],
        now: DateTime<Utc>,
        limit: u32,
    ) -> ProgramRunStoreResult<Vec<ProgramRunActionClaimResultV1>> {
        self.claim_program_run_actions_v1(authority, boot_id, supported_kinds, now, limit)
    }

    pub(crate) fn claim_program_run_dispatch_candidate_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        boot_id: Uuid,
        supported_kinds: &[ProgramRunActionKindV1],
        now: DateTime<Utc>,
        action_id: Uuid,
    ) -> ProgramRunStoreResult<Option<ProgramRunActionClaimResultV1>> {
        if action_id.is_nil() {
            return Err(ProgramRunStoreError::InvalidRequest);
        }
        let mut claims = self.claim_exact_program_run_action_v1(
            authority,
            boot_id,
            supported_kinds,
            now,
            action_id,
        )?;
        Ok(claims.pop())
    }

    fn claim_exact_program_run_action_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        boot_id: Uuid,
        supported_kinds: &[ProgramRunActionKindV1],
        now: DateTime<Utc>,
        action_id: Uuid,
    ) -> ProgramRunStoreResult<Vec<ProgramRunActionClaimResultV1>> {
        if authority.actor_kind != ProgramRunActorKindV1::Scheduler
            || boot_id.is_nil()
            || boot_id != self.program_run_boot_id()
            || supported_kinds.is_empty()
        {
            return Err(ProgramRunStoreError::InvalidRequest);
        }
        let supports_work = supported_kinds.contains(&ProgramRunActionKindV1::Work);
        let supports_wake = supported_kinds.contains(&ProgramRunActionKindV1::Wake);
        let controller_session_id = authority
            .controller_session_id
            .ok_or(ProgramRunStoreError::ControllerMismatch)?;
        let controller_epoch = authority
            .controller_epoch
            .ok_or(ProgramRunStoreError::StaleControllerEpoch)?;
        let tx = immediate(&self.conn)?;
        let candidate = tx
            .query_row(
                PROGRAM_RUN_DISPATCH_EXACT_CANDIDATE_SQL,
                params![
                    action_id.to_string(),
                    controller_session_id.to_string(),
                    to_i64(controller_epoch)?,
                    authority.project_id.map(|id| id.to_string()),
                    authority.idea_id.map(|id| id.to_string()),
                    boot_id.to_string(),
                    i64::from(supports_work),
                    i64::from(supports_wake),
                    timestamp(now),
                ],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()
            .map_err(map_sql)?;
        let Some((current, due)) = candidate else {
            tx.commit().map_err(map_sql)?;
            return Ok(Vec::new());
        };
        if current {
            let result = load_action_claim_result_tx(&tx, action_id, true)?;
            tx.commit().map_err(map_sql)?;
            return Ok(vec![result]);
        }
        if !due {
            tx.commit().map_err(map_sql)?;
            return Ok(Vec::new());
        }
        let result = claim_due_action_tx(&tx, authority, action_id, &boot_id.to_string(), now)?;
        tx.commit().map_err(map_sql)?;
        Ok(result.into_iter().collect())
    }

    fn claim_program_run_actions_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        boot_id: Uuid,
        supported_kinds: &[ProgramRunActionKindV1],
        now: DateTime<Utc>,
        limit: u32,
    ) -> ProgramRunStoreResult<Vec<ProgramRunActionClaimResultV1>> {
        if authority.actor_kind != ProgramRunActorKindV1::Scheduler
            || boot_id.is_nil()
            || boot_id != self.program_run_boot_id()
            || !(1..=32).contains(&limit)
            || supported_kinds.is_empty()
        {
            return Err(ProgramRunStoreError::InvalidRequest);
        }
        let tx = immediate(&self.conn)?;
        let supports_work = supported_kinds.contains(&ProgramRunActionKindV1::Work);
        let supports_wake = supported_kinds.contains(&ProgramRunActionKindV1::Wake);
        let boot_id_text = boot_id.to_string();
        let mut results = Vec::new();
        let mut current = tx
            .prepare(
                "SELECT subject.id FROM idea_program_run_actions AS subject
                 JOIN idea_program_runs AS run ON run.id=subject.program_run_id
                 WHERE subject.controller_session_id=?1 AND subject.controller_epoch=?2
                   AND (?6 IS NULL OR run.project_id=?6)
                   AND (?7 IS NULL OR run.idea_id=?7)
                   AND subject.claim_boot_id=?3
                   AND ((subject.state='published'
                     AND (subject.external_model_invocation_id IS NOT NULL
                       OR subject.external_session_id IS NOT NULL
                       OR subject.scheduled_job_id IS NOT NULL)) OR (
                     subject.state='acknowledged'
                     AND subject.action_kind='wake' AND subject.purpose='retry_wake'
                     AND subject.claim_run_version=run.row_version
                     AND EXISTS(SELECT 1 FROM idea_program_run_transitions transition
                       WHERE transition.id=subject.creating_transition_id
                         AND transition.program_run_id=subject.program_run_id
                         AND transition.resulting_run_version=run.row_version)
                     AND run.status='retry_pending'
                     AND NOT EXISTS(SELECT 1 FROM idea_program_run_actions active
                       WHERE active.program_run_id=subject.program_run_id
                         AND active.state IN ('reserved','claimed','published'))))
                   AND ((?4=1 AND subject.action_kind='work') OR (?5=1 AND subject.action_kind='wake'))
                 ORDER BY subject.not_before,subject.id LIMIT ?8",
            )
            .map_err(map_sql)?;
        let current_ids = current
            .query_map(
                params![
                    authority
                        .controller_session_id
                        .ok_or(ProgramRunStoreError::ControllerMismatch)?
                        .to_string(),
                    to_i64(
                        authority
                            .controller_epoch
                            .ok_or(ProgramRunStoreError::StaleControllerEpoch)?
                    )?,
                    boot_id.to_string(),
                    i64::from(supports_work),
                    i64::from(supports_wake),
                    authority.project_id.map(|id| id.to_string()),
                    authority.idea_id.map(|id| id.to_string()),
                    i64::from(limit),
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(map_sql)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_sql)?;
        drop(current);
        for id in current_ids {
            results.push(load_action_claim_result_tx(&tx, parse_uuid(&id)?, true)?);
        }
        let remaining = limit.saturating_sub(
            u32::try_from(results.len()).map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
        );
        if remaining == 0 {
            tx.commit().map_err(map_sql)?;
            return Ok(results);
        }
        // Scan one extra batch so rows found exhausted while reclaiming cannot
        // indefinitely hide later due work. External publications remain capped
        // by `limit`; the additional bounded scan performs only local settlement.
        let scan_limit = limit.saturating_add(32);
        let mut statement = tx
            .prepare(
                "SELECT subject.id FROM idea_program_run_actions AS subject
                 JOIN idea_program_runs AS run ON run.id=subject.program_run_id
                 WHERE subject.controller_session_id=?1 AND subject.controller_epoch=?2
                   AND (?3 IS NULL OR run.project_id=?3)
                   AND (?4 IS NULL OR run.idea_id=?4)
                   AND ((subject.state='reserved' AND subject.not_before<=?5)
                     OR (subject.state IN ('claimed','published')
                       AND subject.claim_expires_at<=?5
                       AND subject.external_model_invocation_id IS NULL
                       AND subject.external_session_id IS NULL
                       AND subject.scheduled_job_id IS NULL))
                   AND ((?6=1 AND subject.action_kind='work')
                     OR (?7=1 AND subject.action_kind='wake'))
                 ORDER BY subject.not_before,subject.id LIMIT ?8",
            )
            .map_err(map_sql)?;
        let candidates = statement
            .query_map(
                params![
                    authority
                        .controller_session_id
                        .ok_or(ProgramRunStoreError::ControllerMismatch)?
                        .to_string(),
                    to_i64(
                        authority
                            .controller_epoch
                            .ok_or(ProgramRunStoreError::StaleControllerEpoch)?,
                    )?,
                    authority.project_id.map(|id| id.to_string()),
                    authority.idea_id.map(|id| id.to_string()),
                    timestamp(now),
                    i64::from(supports_work),
                    i64::from(supports_wake),
                    i64::from(scan_limit),
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(map_sql)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_sql)?;
        drop(statement);
        for candidate in candidates {
            if results.len()
                >= usize::try_from(limit).map_err(|_| ProgramRunStoreError::CorruptStoredState)?
            {
                break;
            }
            let action_id = parse_uuid(&candidate)?;
            if let Some(result) =
                claim_due_action_tx(&tx, authority, action_id, &boot_id_text, now)?
            {
                results.push(result);
            }
        }
        tx.commit().map_err(map_sql)?;
        Ok(results)
    }

    pub(crate) fn select_program_run_dispatch_visits_v1(
        &self,
        boot_id: Uuid,
        supported_kinds: &[ProgramRunActionKindV1],
        now: DateTime<Utc>,
        limit: u32,
    ) -> ProgramRunStoreResult<ProgramRunDispatchBatchV1> {
        if boot_id.is_nil()
            || boot_id != self.program_run_boot_id()
            || supported_kinds.is_empty()
            || limit != PROGRAM_RUN_DISPATCH_VISIT_LIMIT
        {
            return Err(ProgramRunStoreError::InvalidRequest);
        }
        let supports_work = supported_kinds.contains(&ProgramRunActionKindV1::Work);
        let supports_wake = supported_kinds.contains(&ProgramRunActionKindV1::Wake);
        let boot_id_text = boot_id.to_string();
        let observed_cursor = self
            .conn
            .query_row(
                "SELECT value FROM daemon_settings WHERE key=?1",
                [PROGRAM_RUN_DISPATCH_CURSOR_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sql)?;
        let cursor = ProgramRunDispatchCursorV1::parse(observed_cursor.as_deref());
        let mut visits = Vec::with_capacity(
            usize::try_from(limit).map_err(|_| ProgramRunStoreError::InvalidRequest)?,
        );
        let mut selection_queries = 0_u32;

        for state in PROGRAM_RUN_DISPATCH_STATES {
            let position = cursor.position(state);
            let mut state_visits = 0_u32;
            for wrapping in [false, true] {
                if state_visits >= PROGRAM_RUN_DISPATCH_STATE_QUANTUM
                    || (wrapping && position.is_none())
                {
                    break;
                }
                let cursor_not_before =
                    position.map_or("", |position| position.not_before.as_str());
                let cursor_action_id = position
                    .map(|position| position.action_id.to_string())
                    .unwrap_or_default();
                let remaining = PROGRAM_RUN_DISPATCH_STATE_QUANTUM - state_visits;
                selection_queries = selection_queries.saturating_add(1);
                let cursor_predicate = if wrapping {
                    "subject.not_before<?2 OR (subject.not_before=?2 AND subject.id<=?3)"
                } else {
                    "subject.not_before>?2 OR (subject.not_before=?2 AND subject.id>?3)"
                };
                let scan_sql = program_run_dispatch_scan_sql(cursor_predicate);
                let mut statement = self.conn.prepare(&scan_sql).map_err(map_sql)?;
                let rows = statement
                    .query_map(
                        params![
                            state,
                            cursor_not_before,
                            cursor_action_id,
                            i64::from(remaining),
                        ],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, Option<String>>(5)?,
                                row.get::<_, Option<String>>(6)?,
                                row.get::<_, bool>(7)?,
                                row.get::<_, bool>(8)?,
                                row.get::<_, bool>(9)?,
                            ))
                        },
                    )
                    .map_err(map_sql)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(map_sql)?;
                drop(statement);
                state_visits = state_visits.saturating_add(
                    u32::try_from(rows.len())
                        .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
                );

                for (
                    action_id,
                    not_before,
                    controller_session_id,
                    state,
                    action_kind,
                    claim_boot_id,
                    claim_expires_at,
                    has_reference,
                    current_scope,
                    current_acknowledgement,
                ) in rows
                {
                    let action_id = parse_uuid(&action_id)?;
                    let action_kind = ProgramRunActionKindV1::from_str(&action_kind)
                        .map_err(|_| ProgramRunStoreError::CorruptStoredState)?;
                    let action_state = ProgramRunActionStateV1::from_str(&state)
                        .map_err(|_| ProgramRunStoreError::CorruptStoredState)?;
                    let supported = (supports_work && action_kind == ProgramRunActionKindV1::Work)
                        || (supports_wake && action_kind == ProgramRunActionKindV1::Wake);
                    let due = match action_state {
                        ProgramRunActionStateV1::Reserved => parse_time(&not_before)? <= now,
                        ProgramRunActionStateV1::Claimed | ProgramRunActionStateV1::Published
                            if !has_reference =>
                        {
                            claim_expires_at
                                .as_deref()
                                .map(parse_time)
                                .transpose()?
                                .is_some_and(|expires_at| expires_at <= now)
                        }
                        _ => false,
                    };
                    let current_publication = action_state == ProgramRunActionStateV1::Published
                        && has_reference
                        && claim_boot_id.as_deref() == Some(boot_id_text.as_str());
                    let current_acknowledgement = action_state
                        == ProgramRunActionStateV1::Acknowledged
                        && current_acknowledgement
                        && claim_boot_id.as_deref() == Some(boot_id_text.as_str());
                    let dispatchable = supported
                        && current_scope
                        && (due || current_publication || current_acknowledgement);
                    visits.push(ProgramRunDispatchVisitV1 {
                        action_id,
                        controller_session_id: parse_uuid(&controller_session_id)?,
                        dispatchable,
                        may_attempt_external_effect: dispatchable
                            && !current_publication
                            && !current_acknowledgement,
                        state,
                        not_before,
                    });
                }
                if !wrapping && (position.is_none() || state_visits >= remaining) {
                    break;
                }
            }
        }

        Ok(ProgramRunDispatchBatchV1 {
            visits,
            selection_queries,
            observed_cursor,
            cursor,
        })
    }

    pub(crate) fn advance_program_run_dispatch_cursor_v1(
        &self,
        batch: &ProgramRunDispatchBatchV1,
        visits: &[ProgramRunDispatchVisitV1],
        now: DateTime<Utc>,
    ) -> ProgramRunStoreResult<bool> {
        let tx = immediate(&self.conn)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM daemon_settings WHERE key=?1",
                [PROGRAM_RUN_DISPATCH_CURSOR_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sql)?;
        if current != batch.observed_cursor {
            tx.commit().map_err(map_sql)?;
            return Ok(false);
        }
        let mut cursor = batch.cursor.clone();
        for visit in visits {
            cursor.advance(visit);
        }
        let value =
            serde_json::to_string(&cursor).map_err(|_| ProgramRunStoreError::CursorInvariant)?;
        tx.execute(
            "INSERT INTO daemon_settings (key,value,updated_at) VALUES (?1,?2,?3)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
            params![PROGRAM_RUN_DISPATCH_CURSOR_KEY, value, timestamp(now)],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(true)
    }

    pub(crate) fn record_program_run_publication_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        action_id: Uuid,
        boot_id: Uuid,
        claim_generation: u64,
        now: DateTime<Utc>,
    ) -> ProgramRunStoreResult<ProgramRunActionPublicationResultV1> {
        if boot_id != self.program_run_boot_id() {
            return Err(ProgramRunStoreError::StaleClaimGeneration);
        }
        let tx = immediate(&self.conn)?;
        let action = load_action_tx(&tx, action_id)?;
        validate_action_fence_tx(
            &tx,
            authority,
            &action,
            boot_id,
            claim_generation,
            now,
            true,
        )?;
        if action.state == ProgramRunActionStateV1::Published {
            tx.commit().map_err(map_sql)?;
            return Ok(ProgramRunActionPublicationResultV1 {
                action,
                deduplicated: true,
            });
        }
        if action.state != ProgramRunActionStateV1::Claimed {
            return Err(ProgramRunStoreError::InvalidTransition);
        }
        tx.execute(
            "UPDATE idea_program_run_actions SET state='published',published_at=?1,updated_at=?1
             WHERE id=?2 AND state='claimed' AND claim_generation=?3 AND claim_boot_id=?4",
            params![
                timestamp(now),
                action_id.to_string(),
                to_i64(claim_generation)?,
                boot_id.to_string()
            ],
        )
        .map_err(map_sql)?;
        let action = load_action_tx(&tx, action_id)?;
        tx.commit().map_err(map_sql)?;
        Ok(ProgramRunActionPublicationResultV1 {
            action,
            deduplicated: false,
        })
    }

    pub(crate) fn bind_program_run_external_reference_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        action_id: Uuid,
        boot_id: Uuid,
        claim_generation: u64,
        reference: ProgramRunExternalReferenceV1,
        now: DateTime<Utc>,
    ) -> ProgramRunStoreResult<ProgramRunExternalReferenceResultV1> {
        if boot_id != self.program_run_boot_id() {
            return Err(ProgramRunStoreError::StaleClaimGeneration);
        }
        let tx = immediate(&self.conn)?;
        let action = load_action_tx(&tx, action_id)?;
        validate_action_fence_tx(
            &tx,
            authority,
            &action,
            boot_id,
            claim_generation,
            now,
            true,
        )?;
        if action.state != ProgramRunActionStateV1::Published {
            return Err(ProgramRunStoreError::InvalidTransition);
        }
        let (column, value, existing) = match reference {
            ProgramRunExternalReferenceV1::ModelInvocation(id) => (
                "external_model_invocation_id",
                id,
                action.external_model_invocation_id,
            ),
            ProgramRunExternalReferenceV1::Session(id) => {
                ("external_session_id", id, action.external_session_id)
            }
            ProgramRunExternalReferenceV1::ScheduledJob(id) => {
                ("scheduled_job_id", id, action.scheduled_job_id)
            }
        };
        if value.is_nil() {
            return Err(ProgramRunStoreError::InvalidRequest);
        }
        if let Some(existing) = existing {
            if existing != value {
                return Err(ProgramRunStoreError::DownstreamReplayConflict);
            }
            tx.commit().map_err(map_sql)?;
            return Ok(ProgramRunExternalReferenceResultV1 {
                action,
                deduplicated: true,
            });
        }
        if matches!(reference, ProgramRunExternalReferenceV1::ModelInvocation(_)) {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1)",
                    [value.to_string()],
                    |row| row.get(0),
                )
                .map_err(map_sql)?;
            if !exists {
                return Err(ProgramRunStoreError::NotFound);
            }
        }
        if matches!(reference, ProgramRunExternalReferenceV1::Session(_)) {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    [value.to_string()],
                    |row| row.get(0),
                )
                .map_err(map_sql)?;
            if !exists {
                return Err(ProgramRunStoreError::NotFound);
            }
        }
        if matches!(reference, ProgramRunExternalReferenceV1::ScheduledJob(_)) {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE id=?1)",
                    [value.to_string()],
                    |row| row.get(0),
                )
                .map_err(map_sql)?;
            if !exists {
                return Err(ProgramRunStoreError::NotFound);
            }
        }
        tx.execute(
            &format!("UPDATE idea_program_run_actions SET {column}=?1,updated_at=?2 WHERE id=?3"),
            params![value.to_string(), timestamp(now), action_id.to_string()],
        )
        .map_err(map_sql)?;
        let attempt_column = match reference {
            ProgramRunExternalReferenceV1::ModelInvocation(_) => Some("model_invocation_id"),
            ProgramRunExternalReferenceV1::Session(_) => Some("session_id"),
            ProgramRunExternalReferenceV1::ScheduledJob(_) => None,
        };
        if let Some(attempt_column) = attempt_column {
            let changed = tx
                .execute(
                    &format!(
                        "UPDATE idea_program_run_attempt_refs SET {attempt_column}=?1,state='launched',updated_at=?2
                         WHERE action_id=?3 AND state='reserved'"
                    ),
                    params![value.to_string(), timestamp(now), action_id.to_string()],
                )
                .map_err(map_sql)?;
            if changed != 1 {
                return Err(ProgramRunStoreError::ConstraintViolation);
            }
            consume_reserved_budget_tx(
                &tx,
                action.program_run_id,
                ProgramRunBudgetDimensionV1::WorkAttempts,
                now,
            )?;
        }
        let action = load_action_tx(&tx, action_id)?;
        tx.commit().map_err(map_sql)?;
        Ok(ProgramRunExternalReferenceResultV1 {
            action,
            deduplicated: false,
        })
    }

    pub(crate) fn acknowledge_program_run_action_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        action_id: Uuid,
        boot_id: Uuid,
        claim_generation: u64,
        failure: Option<(&str, &str, bool)>,
        now: DateTime<Utc>,
    ) -> ProgramRunStoreResult<ProgramRunActionAcknowledgementResultV1> {
        let tx = immediate(&self.conn)?;
        let action = load_action_tx(&tx, action_id)?;
        let prepared_failure = failure
            .map(|(error_class, error_message, confirmed_pre_effect)| {
                if error_class.is_empty()
                    || error_class.len() > 64
                    || !error_class.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    })
                {
                    return Err(ProgramRunStoreError::InvalidRequest);
                }
                let message = scrub_message_with_limit(error_message, FAILURE_REPLAY_MESSAGE_LIMIT);
                let stored_message = encode_failure_replay_message(&message, confirmed_pre_effect);
                Ok((error_class, message, stored_message, confirmed_pre_effect))
            })
            .transpose()?;
        if action.state == ProgramRunActionStateV1::Failed {
            // Exact historical replay is authorized against the current run
            // controller, not obsolete publication claim/boot witnesses. The
            // private marker in last_error_message durably covers refund
            // semantics while the typed projection exposes only public text.
            let run = load_run_tx(&tx, action.program_run_id)?;
            let (idea_controller, idea_epoch): (Option<String>, i64) = tx
                .query_row(
                    "SELECT current_controller_session_id,controller_epoch
                     FROM ideas WHERE id=?1",
                    [run.idea_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(map_sql)?;
            if authority.actor_kind != ProgramRunActorKindV1::Scheduler
                || authority.controller_session_id != Some(run.controller_session_id)
                || idea_controller.as_deref()
                    != Some(run.controller_session_id.to_string().as_str())
            {
                return Err(ProgramRunStoreError::ControllerMismatch);
            }
            if authority.controller_epoch != Some(run.controller_epoch)
                || to_u64(idea_epoch)? != run.controller_epoch
            {
                return Err(ProgramRunStoreError::StaleControllerEpoch);
            }
            let Some((error_class, _, stored_message, _)) = prepared_failure.as_ref() else {
                return Err(ProgramRunStoreError::DownstreamReplayConflict);
            };
            if action.last_error_class.as_deref() == Some(*error_class)
                && load_stored_failure_message_tx(&tx, action.id)?.as_deref()
                    == Some(stored_message.as_str())
            {
                tx.commit().map_err(map_sql)?;
                return Ok(ProgramRunActionAcknowledgementResultV1 {
                    action,
                    deduplicated: true,
                });
            }
            return Err(ProgramRunStoreError::DownstreamReplayConflict);
        }
        if boot_id != self.program_run_boot_id() {
            return Err(ProgramRunStoreError::StaleClaimGeneration);
        }
        validate_action_fence_tx(
            &tx,
            authority,
            &action,
            boot_id,
            claim_generation,
            now,
            action.state != ProgramRunActionStateV1::Acknowledged,
        )?;
        if let Some((error_class, _message, stored_message, confirmed_pre_effect)) =
            prepared_failure
        {
            if !matches!(
                action.state,
                ProgramRunActionStateV1::Claimed | ProgramRunActionStateV1::Published
            ) {
                return Err(ProgramRunStoreError::InvalidTransition);
            }
            let changed = tx
                .execute(
                    "UPDATE idea_program_run_actions SET state='failed',last_error_class=?1,
                     last_error_message=?2,updated_at=?3 WHERE id=?4 AND claim_boot_id=?5
                     AND claim_generation=?6 AND state IN ('claimed','published')",
                    params![
                        error_class,
                        stored_message,
                        timestamp(now),
                        action_id.to_string(),
                        boot_id.to_string(),
                        to_i64(claim_generation)?
                    ],
                )
                .map_err(map_sql)?;
            if changed != 1 {
                return Err(ProgramRunStoreError::StaleClaimGeneration);
            }
            if action.action_kind == ProgramRunActionKindV1::Work {
                let reserved_attempts = tx
                    .execute(
                        "UPDATE idea_program_run_attempt_refs SET state='failed',updated_at=?1
                     WHERE action_id=?2 AND state='reserved'",
                        params![timestamp(now), action_id.to_string()],
                    )
                    .map_err(map_sql)?;
                if confirmed_pre_effect && reserved_attempts == 1 {
                    release_reserved_budget_tx(
                        &tx,
                        action.program_run_id,
                        ProgramRunBudgetDimensionV1::WorkAttempts,
                        now,
                    )?;
                }
            }
            let action = load_action_tx(&tx, action_id)?;
            tx.commit().map_err(map_sql)?;
            return Ok(ProgramRunActionAcknowledgementResultV1 {
                action,
                deduplicated: false,
            });
        }
        if action.state == ProgramRunActionStateV1::Acknowledged {
            tx.commit().map_err(map_sql)?;
            return Ok(ProgramRunActionAcknowledgementResultV1 {
                action,
                deduplicated: true,
            });
        }
        if action.state != ProgramRunActionStateV1::Published {
            return Err(ProgramRunStoreError::InvalidTransition);
        }
        tx.execute(
            "UPDATE idea_program_run_actions SET state='acknowledged',acknowledged_at=?1,updated_at=?1
             WHERE id=?2 AND state='published' AND claim_generation=?3 AND claim_boot_id=?4",
            params![timestamp(now), action_id.to_string(), to_i64(claim_generation)?, boot_id.to_string()],
        ).map_err(map_sql)?;
        let action = load_action_tx(&tx, action_id)?;
        tx.commit().map_err(map_sql)?;
        Ok(ProgramRunActionAcknowledgementResultV1 {
            action,
            deduplicated: false,
        })
    }

    #[allow(dead_code)] // Invoked only through the private D05 controller capability.
    pub(crate) fn heartbeat_program_run_locks_v1(
        &self,
        authority: &ProgramRunStoreAuthority,
        run_id: Uuid,
        boot_id: Uuid,
        lease_generation: u64,
        now: DateTime<Utc>,
    ) -> ProgramRunStoreResult<Vec<ProgramRunLockV1>> {
        if boot_id != self.program_run_boot_id() {
            return Err(ProgramRunStoreError::StaleLeaseGeneration);
        }
        let tx = immediate(&self.conn)?;
        let run = load_run_tx(&tx, run_id)?;
        validate_transition_authority(
            authority,
            &run,
            ProgramRunOperationV1::AttemptTerminalObserved,
        )?;
        let expected: i64 = tx.query_row("SELECT count(*) FROM idea_program_run_locks WHERE program_run_id=?1 AND state='held'",
            [run_id.to_string()], |row| row.get(0)).map_err(map_sql)?;
        let changed = tx
            .execute(
                "UPDATE idea_program_run_locks SET heartbeat_at=?1,expires_at=?2
             WHERE program_run_id=?3 AND state='held' AND owner_boot_id=?4 AND lease_generation=?5
               AND controller_session_id=?6 AND controller_epoch=?7",
                params![
                    timestamp(now),
                    timestamp(now + Duration::seconds(30)),
                    run_id.to_string(),
                    boot_id.to_string(),
                    to_i64(lease_generation)?,
                    run.controller_session_id.to_string(),
                    to_i64(run.controller_epoch)?
                ],
            )
            .map_err(map_sql)?;
        if changed != expected as usize {
            return Err(ProgramRunStoreError::StaleLeaseGeneration);
        }
        let locks = load_locks_tx(&tx, run_id, now)?;
        tx.commit().map_err(map_sql)?;
        Ok(locks)
    }

    pub(crate) fn get_program_run_operational_status_v1(
        &self,
        run_id: Uuid,
        controller_a6_live: bool,
        now: DateTime<Utc>,
    ) -> ProgramRunStoreResult<ProgramRunOperationalStatusV1> {
        let tx = self.conn.unchecked_transaction().map_err(map_sql)?;
        let run = load_run_tx(&tx, run_id)?;
        let (idea_controller, idea_controller_epoch): (Option<String>, i64) = tx
            .query_row(
                "SELECT current_controller_session_id,controller_epoch FROM ideas WHERE id=?1",
                [run.idea_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sql)?
            .ok_or(ProgramRunStoreError::NotFound)?;
        let locks = load_locks_tx(&tx, run_id, now)?;
        let active_action = load_active_action_tx(&tx, run_id)?;
        let current_attempt = load_current_attempt_tx(&tx, &run)?;
        let gates = load_gates_tx(&tx, run_id)?;
        let template: rsi_common::program_runs::ProgramRunTemplateV1 =
            serde_json::from_str(&run.template_json)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?;
        let required_gates = run
            .cursor_ordinal
            .and_then(|ordinal| template.cursors.get(ordinal as usize))
            .map(|cursor| {
                cursor
                    .required_gates
                    .iter()
                    .map(|requirement| ProgramRunRequiredGateStatusV1 {
                        gate_key: requirement.gate_key.clone(),
                        policy_key: requirement.policy_key.clone(),
                        policy_version: requirement.policy_version,
                        latest_evaluation: gates
                            .iter()
                            .find(|gate| {
                                Some(gate.cursor_ordinal) == run.cursor_ordinal
                                    && gate.revision_no == run.revision_no
                                    && gate.gate_key == requirement.gate_key
                            })
                            .cloned(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let budgets = load_budgets_tx(&tx, run_id)?;
        if budgets.len() != ProgramRunBudgetDimensionV1::ALL.len() {
            return Err(ProgramRunStoreError::CorruptStoredState);
        }
        let controller_matches_idea = idea_controller.as_deref()
            == Some(run.controller_session_id.to_string().as_str())
            && to_u64(idea_controller_epoch)? == run.controller_epoch;
        let (reconciliation_class, next_action, safe_reason) = classify_run(
            &run,
            &locks,
            active_action.as_ref(),
            current_attempt.as_ref(),
            controller_a6_live && controller_matches_idea,
            now,
        );
        tx.commit().map_err(map_sql)?;
        Ok(ProgramRunOperationalStatusV1 {
            run,
            idea_controller_session_id: idea_controller.as_deref().map(parse_uuid).transpose()?,
            idea_controller_epoch: to_u64(idea_controller_epoch)?,
            controller_a6_live,
            controller_matches_idea,
            locks,
            active_action,
            current_attempt,
            gates,
            required_gates,
            budgets,
            reconciliation_class,
            next_action,
            safe_reason,
        })
    }

    pub(crate) fn reconcile_program_runs_page_v1(
        &self,
        project_id: Option<Uuid>,
        cursor: Option<&ProgramRunPageCursorV1>,
        limit: u32,
        time_budget_ms: u32,
        dry_run: bool,
        now: DateTime<Utc>,
        live_controllers: &HashMap<Uuid, u64>,
        boot_id: Uuid,
    ) -> ProgramRunStoreResult<ProgramRunReconciliationPageV1> {
        if !(1..=256).contains(&limit)
            || !(1..=2_000).contains(&time_budget_ms)
            || boot_id.is_nil()
            || boot_id != self.program_run_boot_id()
        {
            return Err(ProgramRunStoreError::InvalidRequest);
        }
        let started = Instant::now();
        let page = self.list_program_runs_v1(project_id, None, None, cursor, limit)?;
        let mut items = Vec::new();
        let mut deadline_reached = false;
        #[cfg(test)]
        let force_deadline_before_first = d05_take_reconcile_deadline_before_first();
        for run in &page.items {
            if {
                #[cfg(test)]
                {
                    force_deadline_before_first && items.is_empty()
                }
                #[cfg(not(test))]
                {
                    false
                }
            } || started.elapsed() >= StdDuration::from_millis(u64::from(time_budget_ms))
            {
                deadline_reached = true;
                break;
            }
            let mut mutated = false;
            let authority_live =
                live_controllers.get(&run.controller_session_id) == Some(&run.controller_epoch);
            if !dry_run && authority_live {
                let tx = immediate(&self.conn)?;
                let expired_locks = tx
                    .execute(
                        "UPDATE idea_program_run_locks SET state='expired',released_at=?2,
                         release_reason='lease_expired' WHERE program_run_id=?1 AND state='held'
                         AND expires_at<=?2",
                        params![run.id.to_string(), timestamp(now)],
                    )
                    .map_err(map_sql)?;
                let reclaimed = tx
                    .execute(
                        "UPDATE idea_program_run_actions SET state='reserved',claim_boot_id=NULL,
                         claim_generation=claim_generation+1,claim_run_version=NULL,
                         claim_lease_generation=NULL,claimed_at=NULL,claim_expires_at=NULL,
                         updated_at=?1 WHERE program_run_id=?2 AND state='claimed'
                         AND (claim_boot_id!=?3 OR claim_expires_at<=?1)",
                        params![timestamp(now), run.id.to_string(), boot_id.to_string()],
                    )
                    .map_err(map_sql)?;
                let rebound = tx
                    .execute(
                        "UPDATE idea_program_run_actions SET claim_boot_id=?1,
                         claim_generation=claim_generation+1,
                         claim_run_version=(SELECT row_version FROM idea_program_runs
                           WHERE id=idea_program_run_actions.program_run_id),
                         claim_lease_generation=(SELECT COALESCE(MAX(lease_generation),0)
                           FROM idea_program_run_locks
                           WHERE program_run_id=idea_program_run_actions.program_run_id AND state='held'),
                         claim_expires_at=?2,updated_at=?3
                         WHERE program_run_id=?4 AND claim_boot_id!=?1 AND state='published'",
                        params![
                            boot_id.to_string(),
                            timestamp(now + Duration::seconds(30)),
                            timestamp(now),
                            run.id.to_string(),
                        ],
                    )
                    .map_err(map_sql)?;
                let unsettled_acknowledgement: Option<String> = tx
                    .query_row(
                        "SELECT acknowledged.id FROM idea_program_run_actions acknowledged
                         JOIN idea_program_runs run ON run.id=acknowledged.program_run_id
                         WHERE acknowledged.program_run_id=?1 AND run.status='retry_pending'
                           AND acknowledged.action_kind='wake'
                           AND acknowledged.purpose='retry_wake'
                           AND acknowledged.claim_run_version=run.row_version
                           AND acknowledged.creating_transition_id=(SELECT transition.id
                             FROM idea_program_run_transitions transition
                             WHERE transition.program_run_id=run.id
                               AND transition.resulting_run_version=run.row_version)
                           AND acknowledged.state='acknowledged' AND acknowledged.claim_boot_id!=?2
                           AND NOT EXISTS(SELECT 1 FROM idea_program_run_actions active
                             WHERE active.program_run_id=acknowledged.program_run_id
                               AND active.state IN ('reserved','claimed','published'))
                         ORDER BY acknowledged.acknowledged_at DESC,acknowledged.id DESC LIMIT 1",
                        params![run.id.to_string(), boot_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(map_sql)?;
                let rebound_acknowledgement = if let Some(action_id) = unsettled_acknowledgement {
                    tx.execute(
                        "UPDATE idea_program_run_actions SET claim_boot_id=?1,
                         claim_generation=claim_generation+1,
                         claim_run_version=(SELECT row_version FROM idea_program_runs
                           WHERE id=idea_program_run_actions.program_run_id),
                         claim_lease_generation=(SELECT COALESCE(MAX(lease_generation),0)
                           FROM idea_program_run_locks
                           WHERE program_run_id=idea_program_run_actions.program_run_id AND state='held'),
                         claim_expires_at=?2,updated_at=?3 WHERE id=?4 AND state='acknowledged'",
                        params![boot_id.to_string(), timestamp(now + Duration::seconds(30)),
                            timestamp(now), action_id],
                    ).map_err(map_sql)?
                } else {
                    0
                };
                mutated = expired_locks + reclaimed + rebound + rebound_acknowledgement > 0;
                tx.commit().map_err(map_sql)?;

                let refreshed = self
                    .get_program_run_v1(run.id)?
                    .ok_or(ProgramRunStoreError::NotFound)?;
                if expired_locks > 0 {
                    self.apply_program_run_transition_v1(
                        &ProgramRunStoreAuthority::controller(
                            refreshed.controller_session_id,
                            refreshed.controller_epoch,
                        ),
                        &ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                            program_run_id: refreshed.id,
                            expected_run_version: refreshed.row_version,
                            expected_idea_version: refreshed.idea_row_version,
                            operation: ProgramRunOperationV1::ReconciledQuarantine,
                            idempotency_key: format!(
                                "program-run-expired-lock-quarantine:{}:{}",
                                refreshed.id, refreshed.row_version
                            ),
                            reason: Some("held lock lease expired".into()),
                        }),
                    )?;
                    mutated = true;
                } else if refreshed.status == ProgramRunStatusV1::Pending {
                    let max_generation: i64 = self
                        .conn
                        .query_row(
                            "SELECT COALESCE(MAX(lease_generation),0) FROM idea_program_run_locks
                             WHERE program_run_id=?1",
                            [run.id.to_string()],
                            |row| row.get(0),
                        )
                        .map_err(map_sql)?;
                    let transition =
                        ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                            program_run_id: refreshed.id,
                            expected_run_version: refreshed.row_version,
                            expected_idea_version: refreshed.idea_row_version,
                            operation: ProgramRunOperationV1::LocksGranted,
                            idempotency_key: format!(
                                "program-run-lock-reconcile:{}:{max_generation}",
                                refreshed.id
                            ),
                            reason: None,
                        });
                    match self.apply_program_run_transition_v1(
                        &ProgramRunStoreAuthority::controller(
                            refreshed.controller_session_id,
                            refreshed.controller_epoch,
                        ),
                        &transition,
                    ) {
                        Ok(_) => mutated = true,
                        Err(ProgramRunStoreError::LockUnavailable) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            let status = self.get_program_run_operational_status_v1(run.id, authority_live, now)?;
            items.push(ProgramRunReconciliationItemV1 {
                program_run_id: run.id,
                class: status.reconciliation_class,
                next_action: status.next_action,
                mutated,
                safe_reason: status.safe_reason,
            });
        }
        let next_cursor = if deadline_reached {
            items
                .last()
                .and_then(|item| page.items.iter().find(|run| run.id == item.program_run_id))
                .map(|run| ProgramRunPageCursorV1 {
                    updated_at: run.updated_at,
                    id: run.id,
                })
                .or_else(|| cursor.cloned())
                .or_else(|| {
                    Some(ProgramRunPageCursorV1 {
                        updated_at: DateTime::<Utc>::UNIX_EPOCH,
                        id: Uuid::nil(),
                    })
                })
        } else {
            page.next_cursor
        };
        Ok(ProgramRunReconciliationPageV1 {
            items,
            next_cursor,
            deadline_reached,
            dry_run,
        })
    }
}

struct PreparedTransition {
    run_id: Uuid,
    expected_run_version: u64,
    expected_idea_version: u64,
    operation: ProgramRunOperationV1,
    idempotency_key: String,
    reason: Option<String>,
    canonical_json: String,
    fingerprint: String,
}

impl PreparedTransition {
    fn from_input(input: &ProgramRunTransitionInputV1) -> ProgramRunStoreResult<Self> {
        let (
            run_id,
            expected_run_version,
            expected_idea_version,
            operation,
            idempotency_key,
            reason,
            canonical_json,
        ) = match input {
            ProgramRunTransitionInputV1::Simple(request) => {
                request
                    .validate()
                    .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
                (
                    request.program_run_id,
                    request.expected_run_version,
                    request.expected_idea_version,
                    request.operation,
                    request.idempotency_key.clone(),
                    request.reason.clone(),
                    canonical_program_run_json(request)
                        .map_err(|_| ProgramRunStoreError::InvalidRequest)?,
                )
            }
            ProgramRunTransitionInputV1::ClaimAction(request) => {
                request
                    .validate()
                    .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
                (
                    request.program_run_id,
                    request.expected_run_version,
                    request.expected_idea_version,
                    ProgramRunOperationV1::ActionClaimed,
                    request.idempotency_key.clone(),
                    None,
                    canonical_program_run_json(request)
                        .map_err(|_| ProgramRunStoreError::InvalidRequest)?,
                )
            }
            ProgramRunTransitionInputV1::AcknowledgeWake(request) => {
                request
                    .validate()
                    .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
                (
                    request.program_run_id,
                    request.expected_run_version,
                    request.expected_idea_version,
                    ProgramRunOperationV1::WakeAcknowledged,
                    request.idempotency_key.clone(),
                    None,
                    canonical_program_run_json(request)
                        .map_err(|_| ProgramRunStoreError::InvalidRequest)?,
                )
            }
            ProgramRunTransitionInputV1::CommitOutput(request) => {
                if request.program_run_id.is_nil()
                    || request.attempt_id.is_nil()
                    || request.idempotency_key.trim().is_empty()
                    || request.idempotency_key.len() > 256
                {
                    return Err(ProgramRunStoreError::InvalidRequest);
                }
                (
                    request.program_run_id,
                    request.expected_run_version,
                    request.expected_idea_version,
                    ProgramRunOperationV1::AttemptOutputCommitted,
                    request.idempotency_key.clone(),
                    None,
                    canonical_program_run_json(request)
                        .map_err(|_| ProgramRunStoreError::InvalidRequest)?,
                )
            }
            ProgramRunTransitionInputV1::Resume(request) => {
                if request.program_run_id.is_nil()
                    || request.idempotency_key.trim().is_empty()
                    || request.idempotency_key.len() > 256
                    || !valid_digest(&request.changed_evidence_digest)
                    || request.reason.is_empty()
                    || request.reason.len() > 2_048
                {
                    return Err(ProgramRunStoreError::InvalidRequest);
                }
                (
                    request.program_run_id,
                    request.expected_run_version,
                    request.expected_idea_version,
                    ProgramRunOperationV1::OperatorUnblocked,
                    request.idempotency_key.clone(),
                    Some(request.reason.clone()),
                    canonical_program_run_json(request)
                        .map_err(|_| ProgramRunStoreError::InvalidRequest)?,
                )
            }
            ProgramRunTransitionInputV1::Gate(request) => {
                if request.program_run_id.is_nil()
                    || request.idempotency_key.trim().is_empty()
                    || request.idempotency_key.len() > 256
                {
                    return Err(ProgramRunStoreError::InvalidRequest);
                }
                (
                    request.program_run_id,
                    request.expected_run_version,
                    request.expected_idea_version,
                    ProgramRunOperationV1::GateEvaluated,
                    request.idempotency_key.clone(),
                    None,
                    canonical_program_run_json(request)
                        .map_err(|_| ProgramRunStoreError::InvalidRequest)?,
                )
            }
        };
        let fingerprint = program_run_fingerprint(
            &format!("program-run-transition:{}:v1", operation.as_str()),
            canonical_json.as_bytes(),
        );
        Ok(Self {
            run_id,
            expected_run_version,
            expected_idea_version,
            operation,
            idempotency_key,
            reason,
            canonical_json,
            fingerprint,
        })
    }
}

fn validate_transition_authority(
    authority: &ProgramRunStoreAuthority,
    run: &ProgramRunV1,
    operation: ProgramRunOperationV1,
) -> ProgramRunStoreResult<()> {
    let operator_operation = matches!(
        operation,
        ProgramRunOperationV1::OperatorCancelled | ProgramRunOperationV1::OperatorUnblocked
    );
    if operator_operation {
        return if authority.actor_kind == ProgramRunActorKindV1::Operator {
            Ok(())
        } else {
            Err(ProgramRunStoreError::Forbidden)
        };
    }
    if authority.actor_kind == ProgramRunActorKindV1::Operator {
        return Err(ProgramRunStoreError::Forbidden);
    }
    if authority.controller_session_id != Some(run.controller_session_id) {
        return Err(ProgramRunStoreError::ControllerMismatch);
    }
    if authority.controller_epoch != Some(run.controller_epoch) {
        return Err(ProgramRunStoreError::StaleControllerEpoch);
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn consume_budget_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
    dimension: ProgramRunBudgetDimensionV1,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    reserve_budget_tx(tx, run_id, dimension, now)?;
    consume_reserved_budget_tx(tx, run_id, dimension, now)
}

fn reserve_budget_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
    dimension: ProgramRunBudgetDimensionV1,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    let changed = tx.execute(
        "UPDATE idea_program_run_budgets SET reserved_value=reserved_value+1,row_version=row_version+1,updated_at=?1
         WHERE program_run_id=?2 AND dimension=?3 AND reserved_value+used_value < limit_value",
        params![timestamp(now), run_id.to_string(), dimension.as_str()],
    ).map_err(map_sql)?;
    if changed == 1 {
        Ok(())
    } else {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM idea_program_run_budgets WHERE program_run_id=?1 AND dimension=?2)",
            params![run_id.to_string(), dimension.as_str()], |row| row.get(0),
        ).map_err(map_sql)?;
        if exists {
            Err(ProgramRunStoreError::BudgetExhausted)
        } else {
            Err(ProgramRunStoreError::CorruptStoredState)
        }
    }
}

fn consume_reserved_budget_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
    dimension: ProgramRunBudgetDimensionV1,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    let changed = tx
        .execute(
            "UPDATE idea_program_run_budgets SET reserved_value=reserved_value-1,
             used_value=used_value+1,row_version=row_version+1,updated_at=?1
             WHERE program_run_id=?2 AND dimension=?3 AND reserved_value>0",
            params![timestamp(now), run_id.to_string(), dimension.as_str()],
        )
        .map_err(map_sql)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(ProgramRunStoreError::ConstraintViolation)
    }
}

fn release_reserved_budget_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
    dimension: ProgramRunBudgetDimensionV1,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    let changed = tx
        .execute(
            "UPDATE idea_program_run_budgets SET reserved_value=reserved_value-1,
             row_version=row_version+1,updated_at=?1
             WHERE program_run_id=?2 AND dimension=?3 AND reserved_value>0",
            params![timestamp(now), run_id.to_string(), dimension.as_str()],
        )
        .map_err(map_sql)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(ProgramRunStoreError::ConstraintViolation)
    }
}

fn fail_exhausted_publication_tx(
    tx: &Transaction<'_>,
    action: &ProgramRunActionV1,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    let changed = tx
        .execute(
            "UPDATE idea_program_run_actions SET state='failed',
             last_error_class='publication_budget_exhausted',
             last_error_message='publication retry budget exhausted',updated_at=?1
             WHERE id=?2 AND state IN ('reserved','claimed','published')",
            params![timestamp(now), action.id.to_string()],
        )
        .map_err(map_sql)?;
    if changed != 1 {
        return Err(ProgramRunStoreError::StaleClaimGeneration);
    }
    if action.action_kind == ProgramRunActionKindV1::Work {
        // An uncertain publication cannot refund work: conservatively retain
        // the reservation while making the current attempt truthfully failed.
        tx.execute(
            "UPDATE idea_program_run_attempt_refs SET state='failed',updated_at=?1
             WHERE action_id=?2 AND state='reserved'",
            params![timestamp(now), action.id.to_string()],
        )
        .map_err(map_sql)?;
    }
    Ok(())
}

fn grant_all_locks_tx(
    tx: &Transaction<'_>,
    run: &ProgramRunV1,
    boot_id: Uuid,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    let requested: i64 = tx.query_row(
        "SELECT count(*) FROM idea_program_run_locks WHERE program_run_id=?1 AND state='requested'",
        [run.id.to_string()], |row| row.get(0),
    ).map_err(map_sql)?;
    let total: i64 = tx
        .query_row(
            "SELECT count(*) FROM idea_program_run_locks WHERE program_run_id=?1",
            [run.id.to_string()],
            |row| row.get(0),
        )
        .map_err(map_sql)?;
    if requested != total {
        return Err(ProgramRunStoreError::LockUnavailable);
    }
    let blocked: bool = tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM idea_program_run_locks mine
           WHERE mine.program_run_id=?1 AND mine.state='requested' AND (
             EXISTS(SELECT 1 FROM idea_program_run_locks held
                    WHERE held.project_id=mine.project_id AND held.lock_key=mine.lock_key AND held.state='held')
             OR EXISTS(SELECT 1 FROM idea_program_run_locks older
                    WHERE older.project_id=mine.project_id AND older.lock_key=mine.lock_key
                      AND older.state='requested' AND older.queue_sequence < mine.queue_sequence)))",
        [run.id.to_string()], |row| row.get(0),
    ).map_err(map_sql)?;
    if blocked {
        return Err(ProgramRunStoreError::LockUnavailable);
    }
    if total > 0 {
        let expires = now + Duration::seconds(30);
        let changed = tx
            .execute(
                "UPDATE idea_program_run_locks SET state='held',owner_boot_id=?1,
             lease_generation=lease_generation+1,acquired_at=?2,
             heartbeat_at=?2,expires_at=?3 WHERE program_run_id=?4 AND state='requested'
             AND controller_session_id=?5 AND controller_epoch=?6",
                params![
                    boot_id.to_string(),
                    timestamp(now),
                    timestamp(expires),
                    run.id.to_string(),
                    run.controller_session_id.to_string(),
                    to_i64(run.controller_epoch)?
                ],
            )
            .map_err(map_sql)?;
        if changed != total as usize {
            return Err(ProgramRunStoreError::StaleControllerEpoch);
        }
    }
    Ok(())
}

fn validate_held_locks_tx(
    tx: &Transaction<'_>,
    run: &ProgramRunV1,
    authority: &ProgramRunStoreAuthority,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    let invalid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM idea_program_run_locks WHERE program_run_id=?1
         AND (state!='held' OR controller_session_id!=?2 OR controller_epoch!=?3 OR expires_at<=?4))",
        params![run.id.to_string(), run.controller_session_id.to_string(), to_i64(run.controller_epoch)?, timestamp(now)],
        |row| row.get(0),
    ).map_err(map_sql)?;
    if invalid || authority.controller_session_id != Some(run.controller_session_id) {
        Err(ProgramRunStoreError::StaleLeaseGeneration)
    } else {
        Ok(())
    }
}

fn release_locks_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
    reason: &str,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    tx.execute(
        "UPDATE idea_program_run_locks SET state=CASE WHEN state='held' THEN 'released' ELSE 'cancelled' END,
         released_at=?1,release_reason=?2 WHERE program_run_id=?3 AND state IN ('requested','held')",
        params![timestamp(now), reason, run_id.to_string()],
    ).map_err(map_sql)?;
    Ok(())
}

fn finish_active_action_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
    state: ProgramRunActionStateV1,
    now: DateTime<Utc>,
    reason: Option<&str>,
) -> ProgramRunStoreResult<()> {
    let (published_at, acknowledged_at) = match state {
        ProgramRunActionStateV1::Acknowledged => (Some(timestamp(now)), Some(timestamp(now))),
        _ => (None, None),
    };
    tx.execute(
        "UPDATE idea_program_run_actions SET state=?1,claim_boot_id=NULL,claim_expires_at=NULL,
         published_at=COALESCE(published_at,?2),acknowledged_at=?3,last_error_class=?4,
         last_error_message=?5,updated_at=?6
         WHERE program_run_id=?7 AND state IN ('reserved','claimed','published')",
        params![
            state.as_str(),
            published_at,
            acknowledged_at,
            reason.map(|_| "program_transition"),
            reason.map(scrub_message),
            timestamp(now),
            run_id.to_string()
        ],
    )
    .map_err(map_sql)?;
    Ok(())
}

fn insert_action_tx(
    tx: &Transaction<'_>,
    run: &ProgramRunV1,
    transition_id: Uuid,
    kind: ProgramRunActionKindV1,
    purpose: ProgramRunActionPurposeV1,
    not_before: DateTime<Utc>,
    template: &rsi_common::program_runs::ProgramRunTemplateV1,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<ProgramRunActionV1> {
    assert_active_action_cardinality_tx(tx, run.id)?;
    let active_exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM idea_program_run_actions
             WHERE program_run_id=?1 AND state IN ('reserved','claimed','published'))",
            [run.id.to_string()],
            |row| row.get(0),
        )
        .map_err(map_sql)?;
    if active_exists {
        return Err(ProgramRunStoreError::ConstraintViolation);
    }
    let action_id = deterministic_program_run_action_id(transition_id, purpose);
    let payload = canonical_program_run_json(&serde_json::json!({
        "program_run_id": run.id, "cursor_ordinal": run.cursor_ordinal,
        "cursor_key": run.cursor_key, "cursor_phase": run.cursor_phase,
        "revision_no": run.revision_no, "run_version": run.row_version,
        "idea_version": run.idea_row_version, "status": run.status.as_str(),
        "purpose": purpose.as_str(),
    }))
    .map_err(|_| ProgramRunStoreError::InvalidRequest)?;
    let fingerprint = program_run_fingerprint("program-run-action:v1", payload.as_bytes());
    let dedup = format!("program-run-action:{action_id}");
    tx.execute(
        "INSERT INTO idea_program_run_actions
         (id,program_run_id,creating_transition_id,action_kind,purpose,payload_json,request_fingerprint,
          downstream_dedup_key,controller_session_id,controller_epoch,not_before,state,claim_boot_id,
          claim_generation,claimed_at,claim_expires_at,publication_attempts,max_publication_attempts,
          external_model_invocation_id,external_session_id,scheduled_job_id,last_error_class,last_error_message,
          created_at,updated_at,published_at,acknowledged_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'reserved',NULL,1,NULL,NULL,0,?12,
                 NULL,NULL,NULL,NULL,NULL,?13,?13,NULL,NULL)",
        params![action_id.to_string(), run.id.to_string(), transition_id.to_string(), kind.as_str(),
            purpose.as_str(), payload, fingerprint, dedup, run.controller_session_id.to_string(),
            to_i64(run.controller_epoch)?, timestamp(not_before), i64::from(template.max_publication_attempts), timestamp(now)],
    ).map_err(map_insert_sql)?;
    load_action_tx(tx, action_id)
}

fn all_required_gates_passed_tx(
    tx: &Transaction<'_>,
    run: &ProgramRunV1,
    cursor: &rsi_common::program_runs::ProgramRunCursorV1,
) -> ProgramRunStoreResult<bool> {
    for gate in &cursor.required_gates {
        let result: Option<String> = tx.query_row(
            "SELECT result FROM idea_program_run_gates WHERE program_run_id=?1 AND cursor_ordinal=?2
             AND revision_no=?3 AND gate_key=?4 ORDER BY evaluation_no DESC LIMIT 1",
            params![run.id.to_string(), run.cursor_ordinal.map(i64::from), i64::from(run.revision_no), gate.gate_key],
            |row| row.get(0),
        ).optional().map_err(map_sql)?;
        if result.as_deref() != Some("passed") {
            return Ok(false);
        }
    }
    Ok(true)
}

fn next_attempt_no_tx(tx: &Transaction<'_>, run: &ProgramRunV1) -> ProgramRunStoreResult<u32> {
    let value: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(attempt_no),0)+1 FROM idea_program_run_attempt_refs
         WHERE program_run_id=?1 AND cursor_ordinal=?2 AND revision_no=?3",
            params![
                run.id.to_string(),
                run.cursor_ordinal.map(i64::from),
                i64::from(run.revision_no)
            ],
            |row| row.get(0),
        )
        .map_err(map_sql)?;
    to_u32(value)
}

const FAILURE_REPLAY_FALSE_SUFFIX: &str = "\u{001f}rsi-failure-replay-v1:0";
const FAILURE_REPLAY_TRUE_SUFFIX: &str = "\u{001f}rsi-failure-replay-v1:1";
const FAILURE_REPLAY_MESSAGE_LIMIT: usize = 512 - FAILURE_REPLAY_TRUE_SUFFIX.len();

fn scrub_message(value: &str) -> String {
    scrub_message_with_limit(value, 512)
}

fn scrub_message_with_limit(value: &str, byte_limit: usize) -> String {
    let mut result = String::new();
    for character in value.chars().filter(|character| !character.is_control()) {
        if result.len() + character.len_utf8() > byte_limit {
            break;
        }
        result.push(character);
    }
    result
}

fn encode_failure_replay_message(message: &str, confirmed_pre_effect: bool) -> String {
    let suffix = if confirmed_pre_effect {
        FAILURE_REPLAY_TRUE_SUFFIX
    } else {
        FAILURE_REPLAY_FALSE_SUFFIX
    };
    format!("{message}{suffix}")
}

fn decode_failure_replay_message(message: String) -> String {
    message
        .strip_suffix(FAILURE_REPLAY_TRUE_SUFFIX)
        .or_else(|| message.strip_suffix(FAILURE_REPLAY_FALSE_SUFFIX))
        .unwrap_or(&message)
        .to_string()
}

fn load_stored_failure_message_tx(
    tx: &Transaction<'_>,
    action_id: Uuid,
) -> ProgramRunStoreResult<Option<String>> {
    tx.query_row(
        "SELECT last_error_message FROM idea_program_run_actions WHERE id=?1",
        [action_id.to_string()],
        |row| row.get(0),
    )
    .map_err(map_sql)
}

fn validate_action_fence_tx(
    tx: &Transaction<'_>,
    authority: &ProgramRunStoreAuthority,
    action: &ProgramRunActionV1,
    boot_id: Uuid,
    claim_generation: u64,
    now: DateTime<Utc>,
    enforce_projection: bool,
) -> ProgramRunStoreResult<()> {
    if authority.actor_kind != ProgramRunActorKindV1::Scheduler
        || authority.controller_session_id != Some(action.controller_session_id)
    {
        return Err(ProgramRunStoreError::ControllerMismatch);
    }
    if authority.controller_epoch != Some(action.controller_epoch) {
        return Err(ProgramRunStoreError::StaleControllerEpoch);
    }
    if action.claim_boot_id != Some(boot_id) || action.claim_generation != claim_generation {
        return Err(ProgramRunStoreError::StaleClaimGeneration);
    }
    if enforce_projection {
        let run_version: i64 = tx
            .query_row(
                "SELECT row_version FROM idea_program_runs WHERE id=?1",
                [action.program_run_id.to_string()],
                |row| row.get(0),
            )
            .map_err(map_sql)?;
        if action.claim_run_version != Some(to_u64(run_version)?) {
            return Err(ProgramRunStoreError::StaleRunVersion);
        }
        let (lease_generation, stale_lease): (i64, bool) = tx
            .query_row(
                "SELECT COALESCE(MAX(lease_generation),0),
                   EXISTS(SELECT 1 FROM idea_program_run_locks WHERE program_run_id=?1
                     AND state='held' AND (owner_boot_id!=?2 OR expires_at<=?3))
                 FROM idea_program_run_locks WHERE program_run_id=?1 AND state='held'",
                params![
                    action.program_run_id.to_string(),
                    boot_id.to_string(),
                    timestamp(now),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(map_sql)?;
        if stale_lease || action.claim_lease_generation != Some(to_u64(lease_generation)?) {
            return Err(ProgramRunStoreError::StaleLeaseGeneration);
        }
    }
    Ok(())
}

fn validate_semantic_action_witness_tx(
    tx: &Transaction<'_>,
    authority: &ProgramRunStoreAuthority,
    run: &ProgramRunV1,
    action_id: Uuid,
    claim_boot_id: Uuid,
    claim_generation: u64,
    claim_run_version: u64,
    claim_lease_generation: u64,
    expected_kind: ProgramRunActionKindV1,
    expected_state: ProgramRunActionStateV1,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<ProgramRunActionV1> {
    if claim_run_version != run.row_version {
        return Err(ProgramRunStoreError::StaleRunVersion);
    }
    let action = load_action_tx(tx, action_id)?;
    if action.program_run_id != run.id
        || action.action_kind != expected_kind
        || action.state != expected_state
    {
        return Err(ProgramRunStoreError::StaleClaimGeneration);
    }
    if action.claim_run_version != Some(claim_run_version) {
        return Err(ProgramRunStoreError::StaleRunVersion);
    }
    if action.claim_lease_generation != Some(claim_lease_generation) {
        return Err(ProgramRunStoreError::StaleLeaseGeneration);
    }
    let action_authority = ProgramRunStoreAuthority::scheduler_scoped(
        authority
            .controller_session_id
            .ok_or(ProgramRunStoreError::ControllerMismatch)?,
        authority
            .controller_epoch
            .ok_or(ProgramRunStoreError::StaleControllerEpoch)?,
        authority.project_id,
        authority.idea_id,
    );
    validate_action_fence_tx(
        tx,
        &action_authority,
        &action,
        claim_boot_id,
        claim_generation,
        now,
        true,
    )?;
    validate_held_locks_tx(tx, run, authority, now)?;
    let active =
        load_active_action_tx(tx, run.id)?.ok_or(ProgramRunStoreError::StaleClaimGeneration)?;
    if active.id != action_id {
        return Err(ProgramRunStoreError::StaleClaimGeneration);
    }
    Ok(action)
}

fn consume_acknowledged_action_tx(
    tx: &Transaction<'_>,
    action: &ProgramRunActionV1,
    claim_boot_id: Uuid,
    claim_generation: u64,
    claim_run_version: u64,
    claim_lease_generation: u64,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<()> {
    let changed = tx
        .execute(
            "UPDATE idea_program_run_actions SET claim_boot_id=NULL,claim_run_version=NULL,
             claim_lease_generation=NULL,claimed_at=NULL,claim_expires_at=NULL,updated_at=?1
             WHERE id=?2 AND program_run_id=?3 AND state='acknowledged'
               AND claim_boot_id=?4 AND claim_generation=?5
               AND claim_run_version=?6 AND claim_lease_generation=?7",
            params![
                timestamp(now),
                action.id.to_string(),
                action.program_run_id.to_string(),
                claim_boot_id.to_string(),
                to_i64(claim_generation)?,
                to_i64(claim_run_version)?,
                to_i64(claim_lease_generation)?,
            ],
        )
        .map_err(map_sql)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(ProgramRunStoreError::StaleClaimGeneration)
    }
}

fn load_budgets_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
) -> ProgramRunStoreResult<Vec<ProgramRunBudgetV1>> {
    let mut statement = tx
        .prepare(
            "SELECT dimension,limit_value,reserved_value,used_value,row_version,updated_at
         FROM idea_program_run_budgets WHERE program_run_id=?1 ORDER BY dimension",
        )
        .map_err(map_sql)?;
    let rows = statement
        .query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(map_sql)?;
    let mut values = Vec::new();
    for row in rows {
        let (dimension, limit, reserved, used, version, updated) = row.map_err(map_sql)?;
        values.push(ProgramRunBudgetV1 {
            program_run_id: run_id,
            dimension: ProgramRunBudgetDimensionV1::from_str(&dimension)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            limit_value: to_u64(limit)?,
            reserved_value: to_u64(reserved)?,
            used_value: to_u64(used)?,
            row_version: to_u64(version)?,
            updated_at: parse_time(&updated)?,
        });
    }
    Ok(values)
}

fn load_locks_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<Vec<ProgramRunLockV1>> {
    let mut statement = tx.prepare(
        "SELECT id,queue_sequence,project_id,lock_key,conflict_domain,state,controller_session_id,
                controller_epoch,lease_generation,owner_boot_id,requested_at,acquired_at,heartbeat_at,
                expires_at,released_at
         FROM idea_program_run_locks WHERE program_run_id=?1 ORDER BY lock_key",
    ).map_err(map_sql)?;
    let rows = statement
        .query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
            ))
        })
        .map_err(map_sql)?;
    let mut values = Vec::new();
    for row in rows {
        let (
            id,
            sequence,
            project,
            key,
            domain,
            state,
            controller,
            epoch,
            generation,
            boot,
            requested,
            acquired,
            heartbeat,
            expires,
            released,
        ) = row.map_err(map_sql)?;
        let queue_position = if state == "requested" {
            let position: i64 = tx.query_row(
                "SELECT count(*) FROM idea_program_run_locks WHERE project_id=?1 AND lock_key=?2
                 AND state='requested' AND queue_sequence<=?3",
                params![project, key, sequence], |row| row.get(0),
            ).map_err(map_sql)?;
            Some(to_u32(position)?)
        } else {
            None
        };
        let queue_depth: i64 = tx
            .query_row(
                "SELECT count(*) FROM idea_program_run_locks
                 WHERE project_id=?1 AND lock_key=?2 AND state='requested'",
                params![project, key],
                |row| row.get(0),
            )
            .map_err(map_sql)?;
        let expiry_eligible = expires
            .as_deref()
            .map(parse_time)
            .transpose()?
            .is_some_and(|value| value <= now);
        values.push(ProgramRunLockV1 {
            id: parse_uuid(&id)?,
            queue_sequence: to_u64(sequence)?,
            project_id: parse_uuid(&project)?,
            lock_key: key,
            conflict_domain: ProgramRunLockDomainV1::from_str(&domain)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            program_run_id: run_id,
            state: ProgramRunLockStateV1::from_str(&state)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            controller_session_id: parse_uuid(&controller)?,
            controller_epoch: to_u64(epoch)?,
            lease_generation: to_u64(generation)?,
            owner_boot_id: boot.as_deref().map(parse_uuid).transpose()?,
            requested_at: parse_time(&requested)?,
            acquired_at: acquired.as_deref().map(parse_time).transpose()?,
            heartbeat_at: heartbeat.as_deref().map(parse_time).transpose()?,
            expires_at: expires.as_deref().map(parse_time).transpose()?,
            released_at: released.as_deref().map(parse_time).transpose()?,
            queue_position,
            queue_depth: to_u32(queue_depth)?,
            expiry_eligible,
        });
    }
    Ok(values)
}

fn load_gates_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
) -> ProgramRunStoreResult<Vec<ProgramRunGateEvaluationV1>> {
    let mut statement = tx.prepare(
        "SELECT id,cursor_ordinal,cursor_key,revision_no,gate_key,evaluation_no,result,policy_key,
                policy_version,evidence_digest,transition_id,created_at
         FROM idea_program_run_gates AS gate
         WHERE program_run_id=?1
           AND evaluation_no=(SELECT max(newer.evaluation_no) FROM idea_program_run_gates AS newer
                              WHERE newer.program_run_id=gate.program_run_id
                                AND newer.cursor_ordinal=gate.cursor_ordinal
                                AND newer.revision_no=gate.revision_no
                                AND newer.gate_key=gate.gate_key)
         ORDER BY cursor_ordinal,revision_no,gate_key",
    ).map_err(map_sql)?;
    let rows = statement
        .query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
            ))
        })
        .map_err(map_sql)?;
    let mut values = Vec::new();
    for row in rows {
        let (
            id,
            cursor,
            cursor_key,
            revision,
            gate_key,
            evaluation,
            result,
            policy_key,
            policy_version,
            evidence_digest,
            transition,
            created,
        ) = row.map_err(map_sql)?;
        values.push(ProgramRunGateEvaluationV1 {
            id: parse_uuid(&id)?,
            program_run_id: run_id,
            cursor_ordinal: to_u32(cursor)?,
            cursor_key,
            revision_no: to_u32(revision)?,
            gate_key,
            evaluation_no: to_u32(evaluation)?,
            result: ProgramRunGateResultV1::from_str(&result)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            policy_key,
            policy_version: to_u32(policy_version)?,
            evidence_digest,
            transition_id: parse_uuid(&transition)?,
            created_at: parse_time(&created)?,
        });
    }
    Ok(values)
}

fn classify_run(
    run: &ProgramRunV1,
    locks: &[ProgramRunLockV1],
    action: Option<&ProgramRunActionV1>,
    attempt: Option<&ProgramRunAttemptRefV1>,
    authority_live: bool,
    now: DateTime<Utc>,
) -> (
    ProgramRunReconciliationClassV1,
    ProgramRunNextActionV1,
    Option<String>,
) {
    if run.status.is_terminal() {
        return (
            ProgramRunReconciliationClassV1::Terminal,
            ProgramRunNextActionV1::None,
            None,
        );
    }
    if !authority_live {
        return (
            ProgramRunReconciliationClassV1::Blocked,
            ProgramRunNextActionV1::OperatorCancelOnly,
            Some("controller authority is not live".into()),
        );
    }
    let has_expired_held_lock = locks.iter().any(|lock| {
        lock.state == ProgramRunLockStateV1::Held
            && lock.expires_at.is_some_and(|expiry| expiry <= now)
    });
    let action_candidate = match action {
        Some(action)
            if matches!(
                action.state,
                ProgramRunActionStateV1::Claimed | ProgramRunActionStateV1::Published
            ) && !action_has_external_reference(action)
                && action.claim_expires_at.is_some_and(|expiry| expiry <= now) =>
        {
            (
                ProgramRunReconciliationClassV1::RecoverClaim,
                ProgramRunNextActionV1::ReclaimClaim,
                None,
            )
        }
        Some(action)
            if action.purpose == ProgramRunActionPurposeV1::EvaluateGates
                && matches!(
                    action.state,
                    ProgramRunActionStateV1::Reserved | ProgramRunActionStateV1::Claimed
                ) =>
        {
            (
                ProgramRunReconciliationClassV1::RecoverAction,
                ProgramRunNextActionV1::EvaluateGate,
                None,
            )
        }
        Some(action)
            if action.action_kind == ProgramRunActionKindV1::Work
                && action.state == ProgramRunActionStateV1::Reserved =>
        {
            (
                ProgramRunReconciliationClassV1::AwaitExternal,
                ProgramRunNextActionV1::AwaitWorkPublisher,
                None,
            )
        }
        Some(action) if action.state == ProgramRunActionStateV1::Reserved => (
            ProgramRunReconciliationClassV1::RecoverAction,
            ProgramRunNextActionV1::ClaimAction,
            None,
        ),
        Some(action) if action.state == ProgramRunActionStateV1::Claimed => (
            ProgramRunReconciliationClassV1::AwaitExternal,
            ProgramRunNextActionV1::AwaitPublication,
            None,
        ),
        Some(action)
            if (action.state == ProgramRunActionStateV1::Published
                && action_has_external_reference(action)
                || action.state == ProgramRunActionStateV1::Acknowledged)
                && action.action_kind == ProgramRunActionKindV1::Wake =>
        {
            (
                ProgramRunReconciliationClassV1::AwaitExternal,
                ProgramRunNextActionV1::AckWake,
                None,
            )
        }
        Some(_) => (
            ProgramRunReconciliationClassV1::AwaitExternal,
            ProgramRunNextActionV1::AwaitAttempt,
            None,
        ),
        None if run.status == ProgramRunStatusV1::Pending => (
            ProgramRunReconciliationClassV1::RecoverAction,
            ProgramRunNextActionV1::ReconcileLocks,
            None,
        ),
        None if run.status == ProgramRunStatusV1::Blocked => (
            ProgramRunReconciliationClassV1::Blocked,
            ProgramRunNextActionV1::OperatorUnblock,
            None,
        ),
        None if run.status == ProgramRunStatusV1::Running
            && attempt.is_some_and(|attempt| attempt.state == ProgramRunAttemptStateV1::Failed) =>
        {
            (
                ProgramRunReconciliationClassV1::Quarantined,
                ProgramRunNextActionV1::OperatorCancelOnly,
                Some("failed work attempt requires operator cancellation before recovery".into()),
            )
        }
        None if run.status == ProgramRunStatusV1::Running && attempt.is_some() => (
            ProgramRunReconciliationClassV1::AwaitExternal,
            ProgramRunNextActionV1::CommitOutput,
            None,
        ),
        _ => (
            ProgramRunReconciliationClassV1::Quarantined,
            ProgramRunNextActionV1::OperatorCancelOnly,
            Some("nonterminal run has no unique legal next action".into()),
        ),
    };
    if has_expired_held_lock {
        if action.is_some() || run.status != ProgramRunStatusV1::Pending {
            return (
                ProgramRunReconciliationClassV1::Quarantined,
                ProgramRunNextActionV1::OperatorCancelOnly,
                Some("multiple durable next actions are eligible".into()),
            );
        }
        return (
            ProgramRunReconciliationClassV1::RecoverAction,
            ProgramRunNextActionV1::ReconcileLocks,
            Some("held lock lease expired".into()),
        );
    }
    action_candidate
}

struct RunRow {
    id: String,
    project_id: String,
    idea_id: String,
    template_key: String,
    template_version: i64,
    template_digest: String,
    template_json: String,
    status: String,
    cursor_ordinal: Option<i64>,
    cursor_key: Option<String>,
    cursor_phase: Option<String>,
    revision_no: i64,
    controller_session_id: String,
    controller_epoch: i64,
    idea_row_version: i64,
    row_version: i64,
    next_transition_sequence: i64,
    created_at: String,
    updated_at: String,
    settled_at: Option<String>,
    cancelled_at: Option<String>,
    failed_at: Option<String>,
}

fn map_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        idea_id: row.get(2)?,
        template_key: row.get(3)?,
        template_version: row.get(4)?,
        template_digest: row.get(5)?,
        template_json: row.get(6)?,
        status: row.get(7)?,
        cursor_ordinal: row.get(8)?,
        cursor_key: row.get(9)?,
        cursor_phase: row.get(10)?,
        revision_no: row.get(11)?,
        controller_session_id: row.get(12)?,
        controller_epoch: row.get(13)?,
        idea_row_version: row.get(14)?,
        row_version: row.get(15)?,
        next_transition_sequence: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
        settled_at: row.get(19)?,
        cancelled_at: row.get(20)?,
        failed_at: row.get(21)?,
    })
}

impl TryFrom<RunRow> for ProgramRunV1 {
    type Error = ProgramRunStoreError;

    fn try_from(row: RunRow) -> ProgramRunStoreResult<Self> {
        let cursor_ordinal = row.cursor_ordinal.map(to_u32).transpose()?;
        let all_cursor =
            cursor_ordinal.is_some() && row.cursor_key.is_some() && row.cursor_phase.is_some();
        let no_cursor =
            cursor_ordinal.is_none() && row.cursor_key.is_none() && row.cursor_phase.is_none();
        if !all_cursor && !no_cursor {
            return Err(ProgramRunStoreError::CorruptStoredState);
        }
        Ok(Self {
            id: parse_uuid(&row.id)?,
            project_id: parse_uuid(&row.project_id)?,
            idea_id: parse_uuid(&row.idea_id)?,
            template_key: row.template_key,
            template_version: to_u32(row.template_version)?,
            template_digest: row.template_digest,
            template_json: row.template_json,
            status: ProgramRunStatusV1::from_str(&row.status)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            cursor_ordinal,
            cursor_key: row.cursor_key,
            cursor_phase: row.cursor_phase,
            revision_no: to_u32(row.revision_no)?,
            controller_session_id: parse_uuid(&row.controller_session_id)?,
            controller_epoch: to_u64(row.controller_epoch)?,
            idea_row_version: to_u64(row.idea_row_version)?,
            row_version: to_u64(row.row_version)?,
            next_transition_sequence: to_u64(row.next_transition_sequence)?,
            created_at: parse_time(&row.created_at)?,
            updated_at: parse_time(&row.updated_at)?,
            settled_at: row.settled_at.as_deref().map(parse_time).transpose()?,
            cancelled_at: row.cancelled_at.as_deref().map(parse_time).transpose()?,
            failed_at: row.failed_at.as_deref().map(parse_time).transpose()?,
        })
    }
}

struct TransitionRow {
    id: String,
    program_run_id: String,
    sequence: i64,
    operation: String,
    from_status: Option<String>,
    to_status: String,
    old_cursor_ordinal: Option<i64>,
    new_cursor_ordinal: Option<i64>,
    old_revision_no: i64,
    new_revision_no: i64,
    actor_kind: String,
    actor_session_id: Option<String>,
    controller_epoch: i64,
    expected_run_version: i64,
    resulting_run_version: i64,
    expected_idea_version: i64,
    resulting_idea_version: i64,
    idea_event_id: String,
    idempotency_key: String,
    request_fingerprint: String,
    created_at: String,
}

fn map_transition_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TransitionRow> {
    Ok(TransitionRow {
        id: row.get(0)?,
        program_run_id: row.get(1)?,
        sequence: row.get(2)?,
        operation: row.get(3)?,
        from_status: row.get(4)?,
        to_status: row.get(5)?,
        old_cursor_ordinal: row.get(6)?,
        new_cursor_ordinal: row.get(7)?,
        old_revision_no: row.get(8)?,
        new_revision_no: row.get(9)?,
        actor_kind: row.get(10)?,
        actor_session_id: row.get(11)?,
        controller_epoch: row.get(12)?,
        expected_run_version: row.get(13)?,
        resulting_run_version: row.get(14)?,
        expected_idea_version: row.get(15)?,
        resulting_idea_version: row.get(16)?,
        idea_event_id: row.get(17)?,
        idempotency_key: row.get(18)?,
        request_fingerprint: row.get(19)?,
        created_at: row.get(20)?,
    })
}

impl TryFrom<TransitionRow> for ProgramRunTransitionV1 {
    type Error = ProgramRunStoreError;

    fn try_from(row: TransitionRow) -> ProgramRunStoreResult<Self> {
        Ok(Self {
            id: parse_uuid(&row.id)?,
            program_run_id: parse_uuid(&row.program_run_id)?,
            sequence: to_u64(row.sequence)?,
            operation: ProgramRunOperationV1::from_str(&row.operation)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            from_status: row
                .from_status
                .as_deref()
                .map(ProgramRunStatusV1::from_str)
                .transpose()
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            to_status: ProgramRunStatusV1::from_str(&row.to_status)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            old_cursor_ordinal: row.old_cursor_ordinal.map(to_u32).transpose()?,
            new_cursor_ordinal: row.new_cursor_ordinal.map(to_u32).transpose()?,
            old_revision_no: to_u32(row.old_revision_no)?,
            new_revision_no: to_u32(row.new_revision_no)?,
            actor_kind: ProgramRunActorKindV1::from_str(&row.actor_kind)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            actor_session_id: row
                .actor_session_id
                .as_deref()
                .map(parse_uuid)
                .transpose()?,
            controller_epoch: to_u64(row.controller_epoch)?,
            expected_run_version: to_u64(row.expected_run_version)?,
            resulting_run_version: to_u64(row.resulting_run_version)?,
            expected_idea_version: to_u64(row.expected_idea_version)?,
            resulting_idea_version: to_u64(row.resulting_idea_version)?,
            idea_event_id: parse_uuid(&row.idea_event_id)?,
            idempotency_key: row.idempotency_key,
            request_fingerprint: row.request_fingerprint,
            created_at: parse_time(&row.created_at)?,
        })
    }
}

fn load_run_tx(tx: &Transaction<'_>, run_id: Uuid) -> ProgramRunStoreResult<ProgramRunV1> {
    tx.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM idea_program_runs WHERE id=?1"),
        [run_id.to_string()],
        map_run_row,
    )
    .optional()
    .map_err(map_sql)?
    .ok_or(ProgramRunStoreError::NotFound)?
    .try_into()
}

fn load_transition_by_id_tx(
    tx: &Transaction<'_>,
    transition_id: Uuid,
) -> ProgramRunStoreResult<ProgramRunTransitionV1> {
    tx.query_row(
        &format!("SELECT {TRANSITION_COLUMNS} FROM idea_program_run_transitions WHERE id=?1"),
        [transition_id.to_string()],
        map_transition_row,
    )
    .optional()
    .map_err(map_sql)?
    .ok_or(ProgramRunStoreError::CorruptStoredState)?
    .try_into()
}

fn load_replayed_run_projection_tx(
    tx: &Transaction<'_>,
    current: &ProgramRunV1,
    transition: &ProgramRunTransitionV1,
) -> ProgramRunStoreResult<ProgramRunV1> {
    let (cursor_key, cursor_phase): (Option<String>, Option<String>) = tx
        .query_row(
            "SELECT new_cursor_key,new_cursor_phase FROM idea_program_run_transitions WHERE id=?1",
            [transition.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(map_sql)?;
    let next_rebind: Option<String> = tx
        .query_row(
            "SELECT request_json FROM idea_program_run_transitions
             WHERE program_run_id=?1 AND operation='controller_rebound' AND sequence>?2
             ORDER BY sequence LIMIT 1",
            params![current.id.to_string(), to_i64(transition.sequence)?],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sql)?;
    let controller_session_id = next_rebind
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|json| {
            json.get("former_controller_session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
        })
        .unwrap_or(current.controller_session_id);
    let terminal_at = transition
        .to_status
        .is_terminal()
        .then_some(transition.created_at);
    let mut replay = current.clone();
    replay.status = transition.to_status;
    replay.cursor_ordinal = transition.new_cursor_ordinal;
    replay.cursor_key = cursor_key;
    replay.cursor_phase = cursor_phase;
    replay.revision_no = transition.new_revision_no;
    replay.controller_session_id = controller_session_id;
    replay.controller_epoch = transition.controller_epoch;
    replay.idea_row_version = transition.resulting_idea_version;
    replay.row_version = transition.resulting_run_version;
    replay.next_transition_sequence = transition.sequence + 1;
    replay.updated_at = transition.created_at;
    replay.settled_at = (transition.to_status == ProgramRunStatusV1::Settled)
        .then_some(terminal_at)
        .flatten();
    replay.cancelled_at = (transition.to_status == ProgramRunStatusV1::Cancelled)
        .then_some(terminal_at)
        .flatten();
    replay.failed_at = (transition.to_status == ProgramRunStatusV1::Failed)
        .then_some(terminal_at)
        .flatten();
    Ok(replay)
}

fn claim_due_action_tx(
    tx: &Transaction<'_>,
    authority: &ProgramRunStoreAuthority,
    action_id: Uuid,
    boot_id: &str,
    now: DateTime<Utc>,
) -> ProgramRunStoreResult<Option<ProgramRunActionClaimResultV1>> {
    let prior = load_action_tx(tx, action_id)?;
    if authority.controller_session_id != Some(prior.controller_session_id)
        || authority.controller_epoch != Some(prior.controller_epoch)
    {
        return Ok(None);
    }
    if prior.publication_attempts >= prior.max_publication_attempts {
        fail_exhausted_publication_tx(tx, &prior, now)?;
        return Ok(None);
    }
    if prior.publication_attempts > 0
        && let Err(error) = consume_budget_tx(
            tx,
            prior.program_run_id,
            ProgramRunBudgetDimensionV1::ActionPublicationRetries,
            now,
        )
    {
        if error == ProgramRunStoreError::BudgetExhausted {
            fail_exhausted_publication_tx(tx, &prior, now)?;
            return Ok(None);
        }
        return Err(error);
    }
    let changed = tx
        .execute(
            "UPDATE idea_program_run_actions SET state=CASE WHEN state='reserved' THEN 'claimed' ELSE state END,
             claim_boot_id=?1,
             claim_generation=claim_generation+1,claimed_at=?2,claim_expires_at=?3,
             claim_run_version=(SELECT row_version FROM idea_program_runs WHERE id=program_run_id),
             claim_lease_generation=(SELECT COALESCE(MAX(lease_generation),0) FROM idea_program_run_locks
               WHERE program_run_id=idea_program_run_actions.program_run_id AND state='held'),
             publication_attempts=publication_attempts+1,updated_at=?2
             WHERE id=?4 AND ((state='reserved' AND not_before<=?2)
                OR (state IN ('claimed','published') AND claim_expires_at<=?2
                    AND external_model_invocation_id IS NULL
                    AND external_session_id IS NULL AND scheduled_job_id IS NULL))",
            params![
                boot_id,
                timestamp(now),
                timestamp(now + Duration::seconds(30)),
                action_id.to_string()
            ],
        )
        .map_err(map_sql)?;
    if changed != 1 {
        return Ok(None);
    }
    load_action_claim_result_tx(tx, action_id, false).map(Some)
}

const ACTION_COLUMNS: &str = "id,program_run_id,creating_transition_id,action_kind,purpose,request_fingerprint,downstream_dedup_key,controller_session_id,controller_epoch,not_before,state,claim_boot_id,claim_generation,claim_run_version,claim_lease_generation,claimed_at,claim_expires_at,publication_attempts,max_publication_attempts,external_model_invocation_id,external_session_id,scheduled_job_id,last_error_class,last_error_message,created_at,updated_at,published_at,acknowledged_at";

fn load_action_tx(
    tx: &Transaction<'_>,
    action_id: Uuid,
) -> ProgramRunStoreResult<ProgramRunActionV1> {
    tx.query_row(
        &format!("SELECT {ACTION_COLUMNS} FROM idea_program_run_actions WHERE id=?1"),
        [action_id.to_string()],
        map_action_row,
    )
    .optional()
    .map_err(map_sql)?
    .ok_or(ProgramRunStoreError::NotFound)?
    .try_into()
}

fn load_action_claim_result_tx(
    tx: &Transaction<'_>,
    action_id: Uuid,
    deduplicated: bool,
) -> ProgramRunStoreResult<ProgramRunActionClaimResultV1> {
    let action = load_action_tx(tx, action_id)?;
    let semantic_claim = if action.action_kind == ProgramRunActionKindV1::Work
        && action.state == ProgramRunActionStateV1::Claimed
    {
        let run = load_run_tx(tx, action.program_run_id)?;
        if run.status == ProgramRunStatusV1::Ready {
            let claim_boot_id = action
                .claim_boot_id
                .ok_or(ProgramRunStoreError::CorruptStoredState)?;
            let claim_run_version = action
                .claim_run_version
                .ok_or(ProgramRunStoreError::CorruptStoredState)?;
            let claim_lease_generation = action
                .claim_lease_generation
                .ok_or(ProgramRunStoreError::CorruptStoredState)?;
            Some(ClaimProgramRunActionRequestV1 {
                program_run_id: run.id,
                expected_run_version: run.row_version,
                expected_idea_version: run.idea_row_version,
                idempotency_key: format!(
                    "program-run-action-claimed:{}:{}:{}",
                    action.id, claim_boot_id, action.claim_generation
                ),
                action_id: action.id,
                claim_boot_id,
                claim_generation: action.claim_generation,
                claim_run_version,
                claim_lease_generation,
            })
        } else {
            None
        }
    } else {
        None
    };
    Ok(ProgramRunActionClaimResultV1 {
        action,
        semantic_claim,
        deduplicated,
    })
}

fn load_active_action_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
) -> ProgramRunStoreResult<Option<ProgramRunActionV1>> {
    tx.query_row(
        &format!(
            "SELECT {ACTION_COLUMNS} FROM idea_program_run_actions AS subject
             WHERE program_run_id=?1 AND (
               state IN ('reserved','claimed','published') OR
               (state='acknowledged' AND action_kind='wake' AND purpose='retry_wake'
                AND claim_boot_id IS NOT NULL
                AND claim_run_version=(SELECT run.row_version FROM idea_program_runs run
                  WHERE run.id=subject.program_run_id)
                AND EXISTS(SELECT 1 FROM idea_program_run_transitions transition
                  JOIN idea_program_runs run ON run.id=subject.program_run_id
                  WHERE transition.id=subject.creating_transition_id
                    AND transition.program_run_id=subject.program_run_id
                    AND transition.resulting_run_version=run.row_version)
                AND EXISTS(SELECT 1 FROM idea_program_runs run
                  WHERE run.id=subject.program_run_id AND run.status='retry_pending')
                AND NOT EXISTS(
                 SELECT 1 FROM idea_program_run_actions active
                 WHERE active.program_run_id=?1
                   AND active.state IN ('reserved','claimed','published'))))
             ORDER BY CASE state WHEN 'reserved' THEN 0 WHEN 'claimed' THEN 1
                       WHEN 'published' THEN 2 ELSE 3 END, updated_at DESC LIMIT 1"
        ),
        [run_id.to_string()],
        map_action_row,
    )
    .optional()
    .map_err(map_sql)?
    .map(TryInto::try_into)
    .transpose()
}

fn action_has_external_reference(action: &ProgramRunActionV1) -> bool {
    action.external_model_invocation_id.is_some()
        || action.external_session_id.is_some()
        || action.scheduled_job_id.is_some()
}

struct ActionRow {
    id: String,
    run_id: String,
    transition_id: String,
    kind: String,
    purpose: String,
    fingerprint: String,
    dedup: String,
    controller: String,
    epoch: i64,
    not_before: String,
    state: String,
    claim_boot: Option<String>,
    claim_generation: i64,
    claim_run_version: Option<i64>,
    claim_lease_generation: Option<i64>,
    claimed_at: Option<String>,
    claim_expires: Option<String>,
    publication_attempts: i64,
    max_publication_attempts: i64,
    model_invocation: Option<String>,
    session: Option<String>,
    scheduled_job: Option<String>,
    error_class: Option<String>,
    error_message: Option<String>,
    created_at: String,
    updated_at: String,
    published_at: Option<String>,
    acknowledged_at: Option<String>,
}

fn map_action_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionRow> {
    Ok(ActionRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        transition_id: row.get(2)?,
        kind: row.get(3)?,
        purpose: row.get(4)?,
        fingerprint: row.get(5)?,
        dedup: row.get(6)?,
        controller: row.get(7)?,
        epoch: row.get(8)?,
        not_before: row.get(9)?,
        state: row.get(10)?,
        claim_boot: row.get(11)?,
        claim_generation: row.get(12)?,
        claim_run_version: row.get(13)?,
        claim_lease_generation: row.get(14)?,
        claimed_at: row.get(15)?,
        claim_expires: row.get(16)?,
        publication_attempts: row.get(17)?,
        max_publication_attempts: row.get(18)?,
        model_invocation: row.get(19)?,
        session: row.get(20)?,
        scheduled_job: row.get(21)?,
        error_class: row.get(22)?,
        error_message: row.get(23)?,
        created_at: row.get(24)?,
        updated_at: row.get(25)?,
        published_at: row.get(26)?,
        acknowledged_at: row.get(27)?,
    })
}

impl TryFrom<ActionRow> for ProgramRunActionV1 {
    type Error = ProgramRunStoreError;
    fn try_from(row: ActionRow) -> ProgramRunStoreResult<Self> {
        Ok(Self {
            id: parse_uuid(&row.id)?,
            program_run_id: parse_uuid(&row.run_id)?,
            transition_id: parse_uuid(&row.transition_id)?,
            action_kind: ProgramRunActionKindV1::from_str(&row.kind)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            purpose: ProgramRunActionPurposeV1::from_str(&row.purpose)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            request_fingerprint: row.fingerprint,
            downstream_dedup_key: row.dedup,
            controller_session_id: parse_uuid(&row.controller)?,
            controller_epoch: to_u64(row.epoch)?,
            not_before: parse_time(&row.not_before)?,
            state: ProgramRunActionStateV1::from_str(&row.state)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            claim_boot_id: row.claim_boot.as_deref().map(parse_uuid).transpose()?,
            claim_generation: to_u64(row.claim_generation)?,
            claim_run_version: row.claim_run_version.map(to_u64).transpose()?,
            claim_lease_generation: row.claim_lease_generation.map(to_u64).transpose()?,
            claimed_at: row.claimed_at.as_deref().map(parse_time).transpose()?,
            claim_expires_at: row.claim_expires.as_deref().map(parse_time).transpose()?,
            publication_attempts: to_u32(row.publication_attempts)?,
            max_publication_attempts: to_u32(row.max_publication_attempts)?,
            external_model_invocation_id: row
                .model_invocation
                .as_deref()
                .map(parse_uuid)
                .transpose()?,
            external_session_id: row.session.as_deref().map(parse_uuid).transpose()?,
            scheduled_job_id: row.scheduled_job.as_deref().map(parse_uuid).transpose()?,
            last_error_class: row.error_class,
            last_error_message: row.error_message.map(decode_failure_replay_message),
            created_at: parse_time(&row.created_at)?,
            updated_at: parse_time(&row.updated_at)?,
            published_at: row.published_at.as_deref().map(parse_time).transpose()?,
            acknowledged_at: row.acknowledged_at.as_deref().map(parse_time).transpose()?,
        })
    }
}

const ATTEMPT_COLUMNS: &str = "id,program_run_id,action_id,cursor_ordinal,cursor_key,revision_no,attempt_no,state,session_id,model_invocation_id,observed_session_status,observed_at,output_digest,created_at,updated_at";

fn load_current_attempt_tx(
    tx: &Transaction<'_>,
    run: &ProgramRunV1,
) -> ProgramRunStoreResult<Option<ProgramRunAttemptRefV1>> {
    tx.query_row(
        &format!(
            "SELECT {ATTEMPT_COLUMNS} FROM idea_program_run_attempt_refs
                  WHERE program_run_id=?1 AND cursor_ordinal=?2 AND revision_no=?3
                  ORDER BY attempt_no DESC LIMIT 1"
        ),
        params![
            run.id.to_string(),
            run.cursor_ordinal.map(i64::from),
            i64::from(run.revision_no)
        ],
        map_attempt_row,
    )
    .optional()
    .map_err(map_sql)?
    .map(TryInto::try_into)
    .transpose()
}

struct AttemptRow {
    id: String,
    run_id: String,
    action_id: String,
    cursor: i64,
    cursor_key: String,
    revision: i64,
    attempt: i64,
    state: String,
    session: Option<String>,
    model: Option<String>,
    observed_status: Option<String>,
    observed_at: Option<String>,
    output_digest: Option<String>,
    created_at: String,
    updated_at: String,
}

fn map_attempt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptRow> {
    Ok(AttemptRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        action_id: row.get(2)?,
        cursor: row.get(3)?,
        cursor_key: row.get(4)?,
        revision: row.get(5)?,
        attempt: row.get(6)?,
        state: row.get(7)?,
        session: row.get(8)?,
        model: row.get(9)?,
        observed_status: row.get(10)?,
        observed_at: row.get(11)?,
        output_digest: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

impl TryFrom<AttemptRow> for ProgramRunAttemptRefV1 {
    type Error = ProgramRunStoreError;
    fn try_from(row: AttemptRow) -> ProgramRunStoreResult<Self> {
        Ok(Self {
            id: parse_uuid(&row.id)?,
            program_run_id: parse_uuid(&row.run_id)?,
            action_id: parse_uuid(&row.action_id)?,
            cursor_ordinal: to_u32(row.cursor)?,
            cursor_key: row.cursor_key,
            revision_no: to_u32(row.revision)?,
            attempt_no: to_u32(row.attempt)?,
            state: ProgramRunAttemptStateV1::from_str(&row.state)
                .map_err(|_| ProgramRunStoreError::CorruptStoredState)?,
            session_id: row.session.as_deref().map(parse_uuid).transpose()?,
            model_invocation_id: row.model.as_deref().map(parse_uuid).transpose()?,
            observed_session_status: row.observed_status,
            observed_at: row.observed_at.as_deref().map(parse_time).transpose()?,
            output_digest: row.output_digest,
            created_at: parse_time(&row.created_at)?,
            updated_at: parse_time(&row.updated_at)?,
        })
    }
}

fn immediate(connection: &Connection) -> ProgramRunStoreResult<Transaction<'_>> {
    Transaction::new_unchecked(connection, TransactionBehavior::Immediate).map_err(map_sql)
}

fn map_sql(error: rusqlite::Error) -> ProgramRunStoreError {
    if let rusqlite::Error::SqliteFailure(code, _) = &error {
        if code.code == rusqlite::ErrorCode::DatabaseBusy
            || code.code == rusqlite::ErrorCode::DatabaseLocked
        {
            return ProgramRunStoreError::Contention;
        }
        if code.code == rusqlite::ErrorCode::ConstraintViolation {
            return ProgramRunStoreError::ConstraintViolation;
        }
    }
    ProgramRunStoreError::StorageFailure
}

fn map_insert_sql(error: rusqlite::Error) -> ProgramRunStoreError {
    if let rusqlite::Error::SqliteFailure(code, message) = &error
        && code.code == rusqlite::ErrorCode::ConstraintViolation
    {
        let message = message.as_deref().unwrap_or_default();
        if message.contains("one_nonterminal_idea") || message.contains("idea_program_runs.idea_id")
        {
            return ProgramRunStoreError::ActiveRunExists;
        }
    }
    map_sql(error)
}

fn parse_uuid(value: &str) -> ProgramRunStoreResult<Uuid> {
    let parsed = Uuid::parse_str(value).map_err(|_| ProgramRunStoreError::CorruptStoredState)?;
    if parsed.is_nil() || parsed.to_string() != value {
        return Err(ProgramRunStoreError::CorruptStoredState);
    }
    Ok(parsed)
}

fn parse_time(value: &str) -> ProgramRunStoreResult<DateTime<Utc>> {
    if value.len() != 30 || !value.ends_with('Z') {
        return Err(ProgramRunStoreError::CorruptStoredState);
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| ProgramRunStoreError::CorruptStoredState)
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn to_u64(value: i64) -> ProgramRunStoreResult<u64> {
    u64::try_from(value).map_err(|_| ProgramRunStoreError::CorruptStoredState)
}

fn to_u32(value: i64) -> ProgramRunStoreResult<u32> {
    u32::try_from(value).map_err(|_| ProgramRunStoreError::CorruptStoredState)
}

fn to_i64(value: u64) -> ProgramRunStoreResult<i64> {
    i64::try_from(value).map_err(|_| ProgramRunStoreError::InvalidRequest)
}

fn lock_key(project_id: Uuid, idea_id: Uuid, domain: ProgramRunLockDomainV1) -> String {
    match domain {
        ProgramRunLockDomainV1::IdeaController => format!("idea_controller:{idea_id}"),
        ProgramRunLockDomainV1::DangerousMutation => {
            let _ = project_id;
            "dangerous_mutation".to_string()
        }
    }
}

fn assert_active_action_cardinality_tx(
    tx: &Transaction<'_>,
    run_id: Uuid,
) -> ProgramRunStoreResult<()> {
    let count: i64 = tx
        .query_row(
            "SELECT count(*) FROM idea_program_run_actions
             WHERE program_run_id=?1 AND state IN ('reserved','claimed','published')",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .map_err(map_sql)?;
    if count > 1 {
        return Err(ProgramRunStoreError::CorruptStoredState);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::manual_let_else, clippy::unwrap_used)]

    use super::*;
    use rsi_common::program_runs::{
        ProgramRunBudgetLimitsV1, ProgramRunCursorV1, ProgramRunGateRequirementV1,
        ProgramRunLockRequirementV1, ProgramRunTemplateV1,
    };
    use rsi_common::types::{
        AutonomyPolicy, Capture, CaptureSourceKind, ContentAddressedRef, Idea, IdeaActorKind,
        IdeaLifecycle, IdeaStage, Project, SessionStatus, Sha256Digest,
    };

    struct Fixture {
        store: Store,
        project_id: Uuid,
        controller_id: Uuid,
        idea_id: Uuid,
    }

    fn fixture(name: &str) -> Fixture {
        fixture_with_store(Store::open_in_memory().expect("open D05 fixture"), name)
    }

    fn fixture_with_store(store: Store, name: &str) -> Fixture {
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_project(&project).expect("insert project");
        let mut session = crate::store::tests::make_test_session();
        session.project_id = Some(project.id);
        session.status = SessionStatus::Running;
        store.insert_session(&session).expect("insert controller");
        let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).expect("digest");
        let capture = Capture {
            id: Uuid::new_v4(),
            project_id: project.id,
            creator_kind: IdeaActorKind::Operator,
            creator_id: "d05-test".into(),
            captured_at: now,
            source_kind: CaptureSourceKind::OperatorInput,
            raw_content_digest: digest.clone(),
            storage_policy_id: "cas-v1".into(),
            content_ref: ContentAddressedRef::for_digest(&digest),
        };
        let idea = Idea {
            id: Uuid::new_v4(),
            project_id: project.id,
            slug: format!("d05-{}", Uuid::new_v4()),
            sigil: None,
            genesis_capture_id: capture.id,
            genesis_span_start: None,
            genesis_span_end: None,
            genesis_span_digest: None,
            title: name.into(),
            description: "ProgramRun fixture".into(),
            portfolio_summary: "D05".into(),
            lifecycle: IdeaLifecycle::Open,
            stage: IdeaStage::Captured,
            priority: 1,
            autonomy_policy: AutonomyPolicy::CaptureOnly,
            integration_target_ref: "refs/heads/main".into(),
            program_template_policy_id: Some("d05-v1".into()),
            current_controller_session_id: Some(session.id),
            controller_epoch: 1,
            row_version: 0,
            next_event_sequence: 1,
            created_at: now,
            updated_at: now,
            terminal_at: None,
            superseded_at: None,
        };
        store
            .insert_d01_idea_fixture(&capture, &idea)
            .expect("insert Idea fixture");
        Fixture {
            store,
            project_id: project.id,
            controller_id: session.id,
            idea_id: idea.id,
        }
    }

    fn template(locks: Vec<ProgramRunLockDomainV1>) -> ProgramRunTemplateV1 {
        ProgramRunTemplateV1 {
            template_key: "d05-kernel".into(),
            template_version: 1,
            cursors: vec![ProgramRunCursorV1 {
                key: "implement".into(),
                phase: "implementation".into(),
                required_gates: vec![],
                revision_target_ordinal: None,
            }],
            budgets: ProgramRunBudgetLimitsV1 {
                productive_transitions: 8,
                work_attempts: 4,
                launch_retries: 2,
                revisions: 2,
                wake_reservations: 2,
                action_publication_retries: 2,
            },
            locks: locks
                .into_iter()
                .map(|conflict_domain| ProgramRunLockRequirementV1 { conflict_domain })
                .collect(),
            max_publication_attempts: 3,
        }
    }

    fn add_idea(fixture: &Fixture, name: &str) -> Uuid {
        let now = Utc::now();
        let digest = Sha256Digest::parse(format!("sha256:{}", "e".repeat(64))).unwrap();
        let capture = Capture {
            id: Uuid::new_v4(),
            project_id: fixture.project_id,
            creator_kind: IdeaActorKind::Operator,
            creator_id: "d05-cap-test".into(),
            captured_at: now,
            source_kind: CaptureSourceKind::OperatorInput,
            raw_content_digest: digest.clone(),
            storage_policy_id: "cas-v1".into(),
            content_ref: ContentAddressedRef::for_digest(&digest),
        };
        let idea = Idea {
            id: Uuid::new_v4(),
            project_id: fixture.project_id,
            slug: format!("d05-cap-{}", Uuid::new_v4()),
            sigil: None,
            genesis_capture_id: capture.id,
            genesis_span_start: None,
            genesis_span_end: None,
            genesis_span_digest: None,
            title: name.into(),
            description: name.into(),
            portfolio_summary: name.into(),
            lifecycle: IdeaLifecycle::Open,
            stage: IdeaStage::Captured,
            priority: 1,
            autonomy_policy: AutonomyPolicy::CaptureOnly,
            integration_target_ref: "refs/heads/main".into(),
            program_template_policy_id: Some("d05-v1".into()),
            current_controller_session_id: Some(fixture.controller_id),
            controller_epoch: 1,
            row_version: 0,
            next_event_sequence: 1,
            created_at: now,
            updated_at: now,
            terminal_at: None,
            superseded_at: None,
        };
        fixture
            .store
            .insert_d01_idea_fixture(&capture, &idea)
            .unwrap();
        idea.id
    }

    fn create_for_idea(
        fixture: &Fixture,
        idea_id: Uuid,
        key: &str,
        locks: Vec<ProgramRunLockDomainV1>,
    ) -> ProgramRunStoreResult<ProgramRunMutationResultV1> {
        fixture.store.create_program_run_v1(
            &ProgramRunStoreAuthority::operator(),
            &CreateProgramRunRequestV1 {
                idea_id,
                expected_idea_row_version: 0,
                idempotency_key: key.into(),
                template: template(locks),
            },
        )
    }

    fn create(
        fixture: &Fixture,
        key: &str,
        locks: Vec<ProgramRunLockDomainV1>,
    ) -> ProgramRunMutationResultV1 {
        fixture
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: fixture.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: key.into(),
                    template: template(locks),
                },
            )
            .expect("create ProgramRun")
    }

    fn transition(
        run: &ProgramRunV1,
        operation: ProgramRunOperationV1,
        key: &str,
    ) -> ProgramRunTransitionInputV1 {
        ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
            program_run_id: run.id,
            expected_run_version: run.row_version,
            expected_idea_version: run.idea_row_version,
            operation,
            idempotency_key: key.into(),
            reason: None,
        })
    }

    fn claim_transition(
        claim: &ProgramRunActionClaimResultV1,
        key: &str,
    ) -> ProgramRunTransitionInputV1 {
        let mut request = claim
            .semantic_claim
            .clone()
            .expect("work claim carries its semantic witness");
        request.idempotency_key = key.into();
        ProgramRunTransitionInputV1::ClaimAction(request)
    }

    fn wake_ack_transition(
        run: &ProgramRunV1,
        action: &ProgramRunActionV1,
        key: &str,
    ) -> ProgramRunTransitionInputV1 {
        ProgramRunTransitionInputV1::AcknowledgeWake(AcknowledgeProgramRunWakeRequestV1 {
            program_run_id: run.id,
            expected_run_version: run.row_version,
            expected_idea_version: run.idea_row_version,
            idempotency_key: key.into(),
            action_id: action.id,
            claim_boot_id: action.claim_boot_id.expect("wake claim boot"),
            claim_generation: action.claim_generation,
            claim_run_version: action.claim_run_version.expect("wake claim run version"),
            claim_lease_generation: action
                .claim_lease_generation
                .expect("wake claim lease generation"),
        })
    }

    fn ready(fixture: &Fixture, run: &ProgramRunV1, key: &str) -> ProgramRunMutationResultV1 {
        fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &transition(run, ProgramRunOperationV1::LocksGranted, key),
            )
            .unwrap()
    }

    fn launch_current(
        fixture: &Fixture,
        _run: &ProgramRunV1,
        key: &str,
    ) -> (ProgramRunMutationResultV1, ProgramRunAttemptRefV1) {
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot_id = fixture.store.program_run_boot_id();
        let claim = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        let running = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &claim_transition(&claim, &format!("{key}-claimed")),
            )
            .unwrap();
        fixture
            .store
            .record_program_run_publication_v1(
                &scheduler,
                claim.action.id,
                boot_id,
                claim.action.claim_generation,
                Utc::now(),
            )
            .unwrap();
        fixture
            .store
            .bind_program_run_external_reference_v1(
                &scheduler,
                claim.action.id,
                boot_id,
                claim.action.claim_generation,
                ProgramRunExternalReferenceV1::Session(fixture.controller_id),
                Utc::now(),
            )
            .unwrap();
        let attempt = fixture
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap()
            .current_attempt
            .unwrap();
        (running, attempt)
    }

    fn launch_current_in_new_session(
        fixture: &Fixture,
        _run: &ProgramRunV1,
        key: &str,
    ) -> (ProgramRunMutationResultV1, ProgramRunAttemptRefV1) {
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot_id = fixture.store.program_run_boot_id();
        let claim = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        let running = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &claim_transition(&claim, &format!("{key}-claimed")),
            )
            .unwrap();
        let mut attempt_session = crate::store::tests::make_test_session();
        attempt_session.project_id = Some(fixture.project_id);
        attempt_session.status = SessionStatus::Running;
        fixture.store.insert_session(&attempt_session).unwrap();
        fixture
            .store
            .record_program_run_publication_v1(
                &scheduler,
                claim.action.id,
                boot_id,
                claim.action.claim_generation,
                Utc::now(),
            )
            .unwrap();
        fixture
            .store
            .bind_program_run_external_reference_v1(
                &scheduler,
                claim.action.id,
                boot_id,
                claim.action.claim_generation,
                ProgramRunExternalReferenceV1::Session(attempt_session.id),
                Utc::now(),
            )
            .unwrap();
        let attempt = fixture
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap()
            .current_attempt
            .unwrap();
        (running, attempt)
    }

    fn acknowledge_retry_wake(
        fixture: &Fixture,
        retrying: &ProgramRunV1,
        key: &str,
    ) -> ProgramRunMutationResultV1 {
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot_id = fixture.store.program_run_boot_id();
        let wake = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Wake],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap()
            .action;
        let (job, _) = fixture
            .store
            .insert_or_replay_program_run_wake_job(&wake, fixture.project_id)
            .unwrap();
        fixture
            .store
            .record_program_run_publication_v1(
                &scheduler,
                wake.id,
                boot_id,
                wake.claim_generation,
                Utc::now(),
            )
            .unwrap();
        fixture
            .store
            .bind_program_run_external_reference_v1(
                &scheduler,
                wake.id,
                boot_id,
                wake.claim_generation,
                ProgramRunExternalReferenceV1::ScheduledJob(job.id),
                Utc::now(),
            )
            .unwrap();
        fixture
            .store
            .acknowledge_program_run_action_v1(
                &scheduler,
                wake.id,
                boot_id,
                wake.claim_generation,
                None,
                Utc::now(),
            )
            .unwrap();
        fixture
            .store
            .apply_program_run_transition_v1(&scheduler, &wake_ack_transition(retrying, &wake, key))
            .unwrap()
    }

    fn ready_run_with_consumed_acknowledgement_history(
        fixture: &Fixture,
        key: &str,
    ) -> ProgramRunMutationResultV1 {
        let mut history_template = template(vec![]);
        history_template.cursors[0].required_gates = vec![ProgramRunGateRequirementV1 {
            gate_key: "review".into(),
            policy_key: "review-v1".into(),
            policy_version: 1,
        }];
        history_template.cursors[0].revision_target_ordinal = Some(0);
        history_template.budgets = ProgramRunBudgetLimitsV1 {
            productive_transitions: 64,
            work_attempts: 8,
            launch_retries: 8,
            revisions: 8,
            wake_reservations: 8,
            action_publication_retries: 8,
        };
        history_template.max_publication_attempts = 4;
        let created = fixture
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: fixture.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: format!("{key}-create"),
                    template: history_template,
                },
            )
            .unwrap();
        let mut current = ready(fixture, &created.run, &format!("{key}-ready"));
        let controller = ProgramRunStoreAuthority::controller(fixture.controller_id, 1);
        for iteration in 0..2 {
            let (running, attempt) =
                launch_current_in_new_session(fixture, &current.run, &format!("{key}-{iteration}"));
            let awaiting = fixture
                .store
                .apply_program_run_transition_v1(
                    &controller,
                    &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                        program_run_id: running.run.id,
                        attempt_id: attempt.id,
                        expected_run_version: running.run.row_version,
                        expected_idea_version: running.run.idea_row_version,
                        idempotency_key: format!("{key}-output-{iteration}"),
                        output_ref: format!("cas://{key}-output-{iteration}"),
                        output_digest: format!("sha256:{:064x}", iteration + 1),
                    }),
                )
                .unwrap();
            let retrying = fixture
                .store
                .apply_program_run_transition_v1(
                    &controller,
                    &ProgramRunTransitionInputV1::Gate(RecordProgramRunGateRequestV1 {
                        program_run_id: awaiting.run.id,
                        expected_run_version: awaiting.run.row_version,
                        expected_idea_version: awaiting.run.idea_row_version,
                        idempotency_key: format!("{key}-gate-{iteration}"),
                        gate_key: "review".into(),
                        result: ProgramRunGateResultV1::Failed,
                        policy_key: "review-v1".into(),
                        policy_version: 1,
                        evidence_ref: format!("cas://{key}-gate-{iteration}"),
                        evidence_digest: format!("sha256:{:064x}", iteration + 33),
                    }),
                )
                .unwrap();
            current =
                acknowledge_retry_wake(fixture, &retrying.run, &format!("{key}-wake-{iteration}"));
        }
        let history_count: i64 = fixture
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM idea_program_run_actions
                 WHERE program_run_id=?1 AND state='acknowledged'",
                [current.run.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(history_count >= 4);
        current
    }

    fn action_payload(fixture: &Fixture, transition_id: Uuid) -> serde_json::Value {
        let payload: String = fixture
            .store
            .conn
            .query_row(
                "SELECT payload_json FROM idea_program_run_actions
                 WHERE creating_transition_id=?1",
                [transition_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&payload).unwrap()
    }

    #[test]
    fn d05_action_payload_uses_committed_two_cursor_projection() {
        let fixture = fixture("D05 two-cursor payload");
        let mut two = template(vec![]);
        two.cursors.push(ProgramRunCursorV1 {
            key: "verify".into(),
            phase: "verification".into(),
            required_gates: vec![],
            revision_target_ordinal: None,
        });
        let created = fixture
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: fixture.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "two-cursor-create".into(),
                    template: two,
                },
            )
            .unwrap();
        let ready = ready(&fixture, &created.run, "two-ready");
        let (running, attempt) = launch_current(&fixture, &ready.run, "two");
        let advanced = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: running.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    idempotency_key: "two-output".into(),
                    output_ref: "cas://two-output".into(),
                    output_digest: format!("sha256:{}", "1".repeat(64)),
                }),
            )
            .unwrap();
        let payload = action_payload(&fixture, advanced.transition.id);
        assert_eq!(advanced.run.cursor_ordinal, Some(1));
        assert_eq!(payload["cursor_ordinal"], 1);
        assert_eq!(payload["cursor_key"], "verify");
        assert_eq!(payload["cursor_phase"], "verification");
        assert_eq!(payload["revision_no"], 0);
        assert_eq!(payload["run_version"], advanced.run.row_version);
    }

    #[test]
    fn d05_failed_gate_payload_uses_revision_target_projection() {
        let fixture = fixture("D05 failed gate payload");
        let mut gated = template(vec![]);
        gated.cursors[0].required_gates = vec![ProgramRunGateRequirementV1 {
            gate_key: "review".into(),
            policy_key: "review-v1".into(),
            policy_version: 1,
        }];
        gated.cursors[0].revision_target_ordinal = Some(0);
        let created = fixture
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: fixture.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "failed-gate-create".into(),
                    template: gated,
                },
            )
            .unwrap();
        let ready = ready(&fixture, &created.run, "failed-ready");
        let (running, attempt) = launch_current(&fixture, &ready.run, "failed");
        let awaiting = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: running.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    idempotency_key: "failed-output".into(),
                    output_ref: "cas://failed".into(),
                    output_digest: format!("sha256:{}", "2".repeat(64)),
                }),
            )
            .unwrap();
        let awaiting_status = fixture
            .store
            .get_program_run_operational_status_v1(awaiting.run.id, true, Utc::now())
            .unwrap();
        assert_eq!(
            awaiting_status.idea_controller_session_id,
            Some(fixture.controller_id)
        );
        assert_eq!(awaiting_status.idea_controller_epoch, 1);
        assert_eq!(awaiting_status.required_gates.len(), 1);
        assert_eq!(awaiting_status.required_gates[0].gate_key, "review");
        assert!(
            awaiting_status.required_gates[0]
                .latest_evaluation
                .is_none()
        );
        assert_eq!(
            awaiting_status.next_action,
            ProgramRunNextActionV1::EvaluateGate
        );
        let retried = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &ProgramRunTransitionInputV1::Gate(RecordProgramRunGateRequestV1 {
                    program_run_id: awaiting.run.id,
                    expected_run_version: awaiting.run.row_version,
                    expected_idea_version: awaiting.run.idea_row_version,
                    idempotency_key: "failed-gate".into(),
                    gate_key: "review".into(),
                    result: ProgramRunGateResultV1::Failed,
                    policy_key: "review-v1".into(),
                    policy_version: 1,
                    evidence_ref: "cas://review".into(),
                    evidence_digest: format!("sha256:{}", "3".repeat(64)),
                }),
            )
            .unwrap();
        let payload = action_payload(&fixture, retried.transition.id);
        assert_eq!(retried.run.status, ProgramRunStatusV1::RetryPending);
        assert_eq!(payload["cursor_ordinal"], 0);
        assert_eq!(payload["revision_no"], 1);
        assert_eq!(payload["status"], "retry_pending");
        let charged = fixture
            .store
            .get_program_run_operational_status_v1(retried.run.id, true, Utc::now())
            .unwrap()
            .budgets;
        for dimension in [
            ProgramRunBudgetDimensionV1::Revisions,
            ProgramRunBudgetDimensionV1::WakeReservations,
        ] {
            let budget = charged
                .iter()
                .find(|budget| budget.dimension == dimension)
                .unwrap();
            assert_eq!(
                (budget.reserved_value, budget.used_value, budget.row_version),
                (0, 1, 2)
            );
        }

        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot_id = fixture.store.program_run_boot_id();
        let wake = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Wake],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap()
            .action;
        let (job, replayed) = fixture
            .store
            .insert_or_replay_program_run_wake_job(&wake, fixture.project_id)
            .unwrap();
        assert!(!replayed);
        fixture
            .store
            .record_program_run_publication_v1(
                &scheduler,
                wake.id,
                boot_id,
                wake.claim_generation,
                Utc::now(),
            )
            .unwrap();
        let awaiting_expiry = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Wake],
                Utc::now(),
                1,
            )
            .unwrap();
        assert!(
            awaiting_expiry.is_empty(),
            "published actions without a durable reference wait for claim expiry"
        );
        fixture
            .store
            .bind_program_run_external_reference_v1(
                &scheduler,
                wake.id,
                boot_id,
                wake.claim_generation,
                ProgramRunExternalReferenceV1::ScheduledJob(job.id),
                Utc::now(),
            )
            .unwrap();
        let bound = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Wake],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap()
            .action;
        assert_eq!(bound.scheduled_job_id, Some(job.id));
        fixture
            .store
            .acknowledge_program_run_action_v1(
                &scheduler,
                wake.id,
                boot_id,
                wake.claim_generation,
                None,
                Utc::now(),
            )
            .unwrap();
        let acknowledged = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Wake],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap()
            .action;
        assert_eq!(acknowledged.state, ProgramRunActionStateV1::Acknowledged);
        assert!(acknowledged.published_at.is_some());
        assert!(acknowledged.acknowledged_at.is_some());
        let wake_transition = wake_ack_transition(&retried.run, &acknowledged, "wake-semantic");
        let resumed = fixture
            .store
            .apply_program_run_transition_v1(&scheduler, &wake_transition)
            .unwrap();
        assert_eq!(resumed.run.status, ProgramRunStatusV1::Ready);
        let later_claim = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        fixture
            .store
            .apply_program_run_transition_v1(
                &scheduler,
                &claim_transition(&later_claim, "wake-later-progress"),
            )
            .unwrap();
        let exact_replay = fixture
            .store
            .apply_program_run_transition_v1(&scheduler, &wake_transition)
            .unwrap();
        assert!(exact_replay.deduplicated);
        assert_eq!(exact_replay.run, resumed.run);
        let mut changed_witness = wake_transition;
        let ProgramRunTransitionInputV1::AcknowledgeWake(request) = &mut changed_witness else {
            unreachable!("wake helper returns a wake acknowledgement");
        };
        request.claim_generation += 1;
        assert_eq!(
            fixture
                .store
                .apply_program_run_transition_v1(&scheduler, &changed_witness),
            Err(ProgramRunStoreError::ReplayConflict),
        );
        let wake_jobs: i64 = fixture
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM scheduled_jobs WHERE id=?1",
                [job.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(wake_jobs, 1);
    }

    #[test]
    fn d05_passed_gate_payload_uses_advanced_projection() {
        let fixture = fixture("D05 passed gate payload");
        let mut gated = template(vec![]);
        gated.cursors[0].required_gates = vec![ProgramRunGateRequirementV1 {
            gate_key: "review".into(),
            policy_key: "review-v1".into(),
            policy_version: 1,
        }];
        gated.cursors.push(ProgramRunCursorV1 {
            key: "ship".into(),
            phase: "delivery".into(),
            required_gates: vec![],
            revision_target_ordinal: None,
        });
        let created = fixture
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: fixture.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "passed-gate-create".into(),
                    template: gated,
                },
            )
            .unwrap();
        let ready = ready(&fixture, &created.run, "passed-ready");
        let (running, attempt) = launch_current(&fixture, &ready.run, "passed");
        let awaiting = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: running.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    idempotency_key: "passed-output".into(),
                    output_ref: "cas://passed".into(),
                    output_digest: format!("sha256:{}", "4".repeat(64)),
                }),
            )
            .unwrap();
        let advanced = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &ProgramRunTransitionInputV1::Gate(RecordProgramRunGateRequestV1 {
                    program_run_id: awaiting.run.id,
                    expected_run_version: awaiting.run.row_version,
                    expected_idea_version: awaiting.run.idea_row_version,
                    idempotency_key: "passed-gate".into(),
                    gate_key: "review".into(),
                    result: ProgramRunGateResultV1::Passed,
                    policy_key: "review-v1".into(),
                    policy_version: 1,
                    evidence_ref: "cas://review".into(),
                    evidence_digest: format!("sha256:{}", "5".repeat(64)),
                }),
            )
            .unwrap();
        let payload = action_payload(&fixture, advanced.transition.id);
        assert_eq!(advanced.run.cursor_ordinal, Some(1));
        assert_eq!(payload["cursor_key"], "ship");
        assert_eq!(payload["cursor_phase"], "delivery");
        assert_eq!(payload["status"], "ready");
    }

    #[test]
    fn d05_replay_before_stale_and_changed_replay_conflicts() {
        let fixture = fixture("D05 replay");
        let created = create(&fixture, "create-replay", vec![]);
        let stale_replay = fixture
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: fixture.idea_id,
                    expected_idea_row_version: 999,
                    idempotency_key: "create-replay".into(),
                    template: template(vec![]),
                },
            )
            .expect_err("changed replay");
        assert_eq!(stale_replay, ProgramRunStoreError::ReplayConflict);
        let request = transition(&created.run, ProgramRunOperationV1::LocksGranted, "locks");
        let controller = ProgramRunStoreAuthority::controller(fixture.controller_id, 1);
        let first = fixture
            .store
            .apply_program_run_transition_v1(&controller, &request)
            .expect("transition");
        let (running, _) = launch_current(&fixture, &first.run, "replay-later");
        assert_ne!(running.run.row_version, first.run.row_version);
        let create_replay = fixture
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: fixture.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "create-replay".into(),
                    template: template(vec![]),
                },
            )
            .expect("exact create replay after later transitions");
        assert!(create_replay.deduplicated);
        assert_eq!(create_replay.run, created.run);
        let replay = fixture
            .store
            .apply_program_run_transition_v1(&controller, &request)
            .expect("exact replay after later transition");
        assert!(replay.deduplicated);
        assert_eq!(first.transition.id, replay.transition.id);
        assert_eq!(replay.run, first.run);
        let mut changed = match request {
            ProgramRunTransitionInputV1::Simple(value) => value,
            _ => unreachable!(),
        };
        changed.reason = Some("changed".into());
        assert_eq!(
            fixture
                .store
                .apply_program_run_transition_v1(
                    &controller,
                    &ProgramRunTransitionInputV1::Simple(changed)
                )
                .unwrap_err(),
            ProgramRunStoreError::ReplayConflict
        );
    }

    #[test]
    fn d05_semantic_failpoints_are_all_old_or_all_new_with_one_linked_idea_event() {
        fn snapshot(fixture: &Fixture, run_id: Uuid) -> Vec<String> {
            [
                "SELECT status||':'||row_version||':'||idea_row_version FROM idea_program_runs WHERE id=?1",
                "SELECT CAST(count(*) AS TEXT) FROM idea_program_run_transitions WHERE program_run_id=?1",
                "SELECT CAST(count(*) AS TEXT) FROM idea_program_run_actions WHERE program_run_id=?1",
                "SELECT CAST(count(*) AS TEXT) FROM idea_program_run_attempt_refs WHERE program_run_id=?1",
                "SELECT group_concat(state||':'||lease_generation,',') FROM idea_program_run_locks WHERE program_run_id=?1 ORDER BY queue_sequence",
                "SELECT group_concat(dimension||':'||reserved_value||':'||used_value,',') FROM idea_program_run_budgets WHERE program_run_id=?1 ORDER BY dimension",
            ]
            .iter()
            .map(|query| {
                fixture
                    .store
                    .conn
                    .query_row(query, [run_id.to_string()], |row| {
                        Ok(row.get::<_, Option<String>>(0)?.unwrap_or_default())
                    })
                    .unwrap()
            })
            .chain(std::iter::once(
                fixture
                    .store
                    .conn
                    .query_row(
                        "SELECT row_version||':'||next_event_sequence||':'||
                                (SELECT count(*) FROM idea_events WHERE idea_id=?1)
                         FROM ideas WHERE id=?1",
                        [fixture.idea_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
            ))
            .collect()
        }

        for fault in [
            D05SemanticFault::AfterMutationFacts,
            D05SemanticFault::AfterBudget,
            D05SemanticFault::AfterRunProjection,
            D05SemanticFault::AfterIdeaProjection,
            D05SemanticFault::AfterIdeaEvent,
            D05SemanticFault::AfterTransition,
            D05SemanticFault::AfterAction,
        ] {
            let fixture = fixture(&format!("D05 atomic {fault:?}"));
            let created = create(
                &fixture,
                "atomic-create",
                vec![ProgramRunLockDomainV1::IdeaController],
            );
            let before = snapshot(&fixture, created.run.id);
            d05_test_fail_next_semantic(fault);
            let error = fixture
                .store
                .apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                    &transition(
                        &created.run,
                        ProgramRunOperationV1::LocksGranted,
                        "atomic-locks",
                    ),
                )
                .unwrap_err();
            assert_eq!(error, ProgramRunStoreError::StorageFailure, "{fault:?}");
            assert_eq!(snapshot(&fixture, created.run.id), before, "{fault:?}");
        }

        // The same post-domain-fact seam rolls back a newly reserved attempt,
        // not only lock mutations.
        let fixture = fixture("D05 atomic attempt");
        let created = create(&fixture, "attempt-create", vec![]);
        let controller = ProgramRunStoreAuthority::controller(fixture.controller_id, 1);
        let ready = fixture
            .store
            .apply_program_run_transition_v1(
                &controller,
                &transition(
                    &created.run,
                    ProgramRunOperationV1::LocksGranted,
                    "attempt-ready",
                ),
            )
            .unwrap();
        let claims = fixture
            .store
            .claim_due_program_run_actions_v1(
                &ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1),
                fixture.store.program_run_boot_id(),
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap();
        assert_eq!(claims.len(), 1);
        let before_attempt = snapshot(&fixture, ready.run.id);
        d05_test_fail_next_semantic(D05SemanticFault::AfterMutationFacts);
        assert_eq!(
            fixture
                .store
                .apply_program_run_transition_v1(
                    &controller,
                    &claim_transition(&claims[0], "attempt-reserved"),
                )
                .unwrap_err(),
            ProgramRunStoreError::StorageFailure
        );
        assert_eq!(snapshot(&fixture, ready.run.id), before_attempt);

        let success = fixture
            .store
            .apply_program_run_transition_v1(
                &controller,
                &claim_transition(&claims[0], "attempt-reserved"),
            )
            .unwrap();
        let linked: (i64, i64) = fixture
            .store
            .conn
            .query_row(
                "SELECT
                    (SELECT count(*) FROM idea_program_run_transitions WHERE id=?1 AND idea_event_id=?2),
                    (SELECT count(*) FROM idea_events WHERE id=?2)",
                params![success.transition.id.to_string(), success.idea_event_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(linked, (1, 1));
    }

    #[test]
    fn d05_duplicate_master_action_claim_and_active_action_constraints_hold() {
        let fixture = fixture("D05 claims");
        let created = create(&fixture, "claims-create", vec![]);
        let controller = ProgramRunStoreAuthority::controller(fixture.controller_id, 1);
        let ready = fixture
            .store
            .apply_program_run_transition_v1(
                &controller,
                &transition(
                    &created.run,
                    ProgramRunOperationV1::LocksGranted,
                    "claims-ready",
                ),
            )
            .unwrap();
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let first_boot = fixture.store.program_run_boot_id();
        let first = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                first_boot,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                32,
            )
            .unwrap();
        let forged_boot = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                Uuid::new_v4(),
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                32,
            )
            .unwrap_err();
        assert_eq!(first.len(), 1);
        assert_eq!(forged_boot, ProgramRunStoreError::InvalidRequest);
        let active: i64 = fixture.store.conn.query_row(
            "SELECT count(*) FROM idea_program_run_actions WHERE program_run_id=?1 AND state IN ('reserved','claimed','published')",
            [ready.run.id.to_string()], |row| row.get(0)).unwrap();
        assert_eq!(active, 1);
        fixture
            .store
            .record_program_run_publication_v1(
                &scheduler,
                first[0].action.id,
                first_boot,
                first[0].action.claim_generation,
                Utc::now(),
            )
            .unwrap();
        assert_eq!(
            fixture
                .store
                .bind_program_run_external_reference_v1(
                    &scheduler,
                    first[0].action.id,
                    first_boot,
                    first[0].action.claim_generation,
                    ProgramRunExternalReferenceV1::ModelInvocation(Uuid::new_v4()),
                    Utc::now(),
                )
                .unwrap_err(),
            ProgramRunStoreError::NotFound
        );
        let failed = fixture
            .store
            .acknowledge_program_run_action_v1(
                &scheduler,
                first[0].action.id,
                first_boot,
                first[0].action.claim_generation,
                Some((
                    "downstream_replay_conflict",
                    "changed downstream envelope",
                    true,
                )),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(failed.action.state, ProgramRunActionStateV1::Failed);
        assert_eq!(
            fixture
                .store
                .get_program_run_operational_status_v1(ready.run.id, true, Utc::now())
                .unwrap()
                .reconciliation_class,
            ProgramRunReconciliationClassV1::Quarantined
        );
        assert!(
            fixture
                .store
                .claim_due_program_run_actions_v1(
                    &scheduler,
                    first_boot,
                    &[ProgramRunActionKindV1::Work],
                    Utc::now(),
                    32,
                )
                .unwrap()
                .is_empty(),
            "definitive failure must not manufacture a replacement action"
        );
    }

    #[test]
    fn d05_expired_claim_reconciles_and_reclaims_same_action_identity() {
        let fixture = fixture("D05 expired claim");
        let created = create(&fixture, "expired-create", vec![]);
        let ready = ready(&fixture, &created.run, "expired-ready");
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot_id = fixture.store.program_run_boot_id();
        let first = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        fixture
            .store
            .conn
            .execute(
                "UPDATE idea_program_run_actions SET claim_expires_at=?1 WHERE id=?2",
                params![
                    timestamp(Utc::now() - Duration::seconds(1)),
                    first.action.id.to_string()
                ],
            )
            .unwrap();
        fixture
            .store
            .reconcile_program_runs_page_v1(
                Some(fixture.project_id),
                None,
                200,
                2_000,
                false,
                Utc::now(),
                &HashMap::from([(fixture.controller_id, 1)]),
                boot_id,
            )
            .unwrap();
        let reconciled = fixture
            .store
            .get_program_run_operational_status_v1(ready.run.id, true, Utc::now())
            .unwrap()
            .active_action
            .unwrap();
        assert_eq!(reconciled.id, first.action.id);
        assert_eq!(reconciled.state, ProgramRunActionStateV1::Reserved);
        assert_eq!(
            reconciled.claim_generation,
            first.action.claim_generation + 1
        );
        let reclaimed = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap()
            .action;
        assert_eq!(reclaimed.id, first.action.id);
        assert_eq!(
            reclaimed.claim_generation,
            first.action.claim_generation + 2
        );
        let retry_budget = fixture
            .store
            .get_program_run_operational_status_v1(ready.run.id, true, Utc::now())
            .unwrap()
            .budgets
            .into_iter()
            .find(|budget| {
                budget.dimension == ProgramRunBudgetDimensionV1::ActionPublicationRetries
            })
            .unwrap();
        assert_eq!(
            (
                retry_budget.reserved_value,
                retry_budget.used_value,
                retry_budget.row_version
            ),
            (0, 1, 2)
        );
    }

    #[test]
    fn d05_deadline_before_first_preserves_cursor_and_offline_controller_is_visible() {
        let fixture = fixture("D05 reconciliation deadline");
        let created = create(&fixture, "deadline-create", vec![]);
        let before = fixture.store.conn.total_changes();
        d05_test_force_reconcile_deadline_before_first();
        let deadline = fixture
            .store
            .reconcile_program_runs_page_v1(
                Some(fixture.project_id),
                None,
                256,
                1,
                false,
                Utc::now(),
                &HashMap::new(),
                fixture.store.program_run_boot_id(),
            )
            .unwrap();
        assert!(deadline.deadline_reached);
        assert!(deadline.items.is_empty());
        assert_eq!(fixture.store.conn.total_changes(), before);
        let cursor = deadline.next_cursor.expect("deadline restart cursor");
        assert_eq!(cursor.updated_at, DateTime::<Utc>::UNIX_EPOCH);
        assert!(cursor.id.is_nil());

        let resumed = fixture
            .store
            .reconcile_program_runs_page_v1(
                Some(fixture.project_id),
                Some(&cursor),
                256,
                2_000,
                false,
                Utc::now(),
                &HashMap::new(),
                fixture.store.program_run_boot_id(),
            )
            .unwrap();
        assert_eq!(resumed.items.len(), 1);
        assert_eq!(resumed.items[0].program_run_id, created.run.id);
        assert!(!resumed.items[0].mutated);
        assert_eq!(
            resumed.items[0].class,
            ProgramRunReconciliationClassV1::Blocked
        );
        assert_eq!(
            resumed.items[0].next_action,
            ProgramRunNextActionV1::OperatorCancelOnly
        );
    }

    #[test]
    fn d05_fifo_lock_caps_expiry_and_aba_are_fenced() {
        let first = fixture("D05 FIFO first");
        let first_run = create(
            &first,
            "fifo-first",
            vec![ProgramRunLockDomainV1::DangerousMutation],
        );
        let second = {
            let now = Utc::now();
            let digest = Sha256Digest::parse(format!("sha256:{}", "b".repeat(64))).unwrap();
            let capture = Capture {
                id: Uuid::new_v4(),
                project_id: first.project_id,
                creator_kind: IdeaActorKind::Operator,
                creator_id: "d05-test".into(),
                captured_at: now,
                source_kind: CaptureSourceKind::OperatorInput,
                raw_content_digest: digest.clone(),
                storage_policy_id: "cas-v1".into(),
                content_ref: ContentAddressedRef::for_digest(&digest),
            };
            let idea = Idea {
                id: Uuid::new_v4(),
                project_id: first.project_id,
                slug: format!("fifo-{}", Uuid::new_v4()),
                sigil: None,
                genesis_capture_id: capture.id,
                genesis_span_start: None,
                genesis_span_end: None,
                genesis_span_digest: None,
                title: "second".into(),
                description: "second".into(),
                portfolio_summary: "second".into(),
                lifecycle: IdeaLifecycle::Open,
                stage: IdeaStage::Captured,
                priority: 1,
                autonomy_policy: AutonomyPolicy::CaptureOnly,
                integration_target_ref: "refs/heads/main".into(),
                program_template_policy_id: Some("d05-v1".into()),
                current_controller_session_id: Some(first.controller_id),
                controller_epoch: 1,
                row_version: 0,
                next_event_sequence: 1,
                created_at: now,
                updated_at: now,
                terminal_at: None,
                superseded_at: None,
            };
            first
                .store
                .insert_d01_idea_fixture(&capture, &idea)
                .unwrap();
            first
                .store
                .create_program_run_v1(
                    &ProgramRunStoreAuthority::operator(),
                    &CreateProgramRunRequestV1 {
                        idea_id: idea.id,
                        expected_idea_row_version: 0,
                        idempotency_key: "fifo-second".into(),
                        template: template(vec![
                            ProgramRunLockDomainV1::DangerousMutation,
                            ProgramRunLockDomainV1::IdeaController,
                        ]),
                    },
                )
                .unwrap()
        };
        let controller = ProgramRunStoreAuthority::controller(first.controller_id, 1);
        assert_eq!(
            first
                .store
                .apply_program_run_transition_v1(
                    &controller,
                    &transition(
                        &second.run,
                        ProgramRunOperationV1::LocksGranted,
                        "second-first"
                    )
                )
                .unwrap_err(),
            ProgramRunStoreError::LockUnavailable
        );
        let held = first
            .store
            .apply_program_run_transition_v1(
                &controller,
                &transition(
                    &first_run.run,
                    ProgramRunOperationV1::LocksGranted,
                    "first-held",
                ),
            )
            .unwrap();
        let held_lock = first
            .store
            .get_program_run_operational_status_v1(held.run.id, true, Utc::now())
            .unwrap()
            .locks
            .pop()
            .unwrap();
        assert_eq!(
            held_lock.owner_boot_id,
            Some(first.store.program_run_boot_id())
        );
        assert_eq!(held_lock.lease_generation, 2);
        first
            .store
            .heartbeat_program_run_locks_v1(
                &controller,
                held.run.id,
                first.store.program_run_boot_id(),
                held_lock.lease_generation,
                Utc::now(),
            )
            .unwrap();
        let stale = first.store.heartbeat_program_run_locks_v1(
            &controller,
            held.run.id,
            Uuid::new_v4(),
            99,
            Utc::now(),
        );
        assert_eq!(
            stale.unwrap_err(),
            ProgramRunStoreError::StaleLeaseGeneration
        );
        let expired_at = Utc::now() - Duration::seconds(1);
        first.store.conn.execute(
            "UPDATE idea_program_run_locks SET expires_at=?1 WHERE program_run_id=?2 AND state='held'",
            params![timestamp(expired_at), held.run.id.to_string()],
        ).unwrap();
        let live = HashMap::from([(first.controller_id, 1)]);
        for _ in 0..2 {
            first
                .store
                .reconcile_program_runs_page_v1(
                    Some(first.project_id),
                    None,
                    256,
                    2_000,
                    false,
                    Utc::now(),
                    &live,
                    first.store.program_run_boot_id(),
                )
                .unwrap();
        }
        assert_eq!(
            first
                .store
                .get_program_run_v1(held.run.id)
                .unwrap()
                .unwrap()
                .status,
            ProgramRunStatusV1::Blocked
        );
        assert_eq!(
            first
                .store
                .get_program_run_v1(second.run.id)
                .unwrap()
                .unwrap()
                .status,
            ProgramRunStatusV1::Ready
        );
        let second_locks = first
            .store
            .get_program_run_operational_status_v1(second.run.id, true, Utc::now())
            .unwrap()
            .locks;
        assert_eq!(second_locks.len(), 2);
        assert!(second_locks.iter().all(|lock| {
            lock.state == ProgramRunLockStateV1::Held
                && lock.owner_boot_id == Some(first.store.program_run_boot_id())
        }));
    }

    #[test]
    fn d05_concurrent_lock_grants_preserve_fifo_winner() {
        let fixture = fixture("D05 concurrent FIFO");
        let older = create(
            &fixture,
            "concurrent-older",
            vec![ProgramRunLockDomainV1::DangerousMutation],
        );
        let newer_idea = add_idea(&fixture, "concurrent-newer");
        let newer = create_for_idea(
            &fixture,
            newer_idea,
            "concurrent-newer",
            vec![ProgramRunLockDomainV1::DangerousMutation],
        )
        .unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(fixture.store));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let controller_id = fixture.controller_id;
        let (older_result, newer_result) = std::thread::scope(|scope| {
            let older_store = std::sync::Arc::clone(&store);
            let older_barrier = std::sync::Arc::clone(&barrier);
            let older_thread = scope.spawn(move || {
                older_barrier.wait();
                older_store.lock().unwrap().apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::controller(controller_id, 1),
                    &transition(
                        &older.run,
                        ProgramRunOperationV1::LocksGranted,
                        "concurrent-older-grant",
                    ),
                )
            });
            let newer_store = std::sync::Arc::clone(&store);
            let newer_barrier = std::sync::Arc::clone(&barrier);
            let newer_thread = scope.spawn(move || {
                newer_barrier.wait();
                newer_store.lock().unwrap().apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::controller(controller_id, 1),
                    &transition(
                        &newer.run,
                        ProgramRunOperationV1::LocksGranted,
                        "concurrent-newer-grant",
                    ),
                )
            });
            (older_thread.join().unwrap(), newer_thread.join().unwrap())
        });
        assert_eq!(older_result.unwrap().run.status, ProgramRunStatusV1::Ready);
        assert_eq!(
            newer_result.unwrap_err(),
            ProgramRunStoreError::LockUnavailable
        );
    }

    #[test]
    fn d05_lock_queue_caps_fail_closed_through_create_path() {
        let per_key = fixture("D05 per-key cap");
        for index in 0..64 {
            let idea_id = add_idea(&per_key, &format!("per-key-{index}"));
            create_for_idea(
                &per_key,
                idea_id,
                &format!("per-key-{index}"),
                vec![ProgramRunLockDomainV1::DangerousMutation],
            )
            .unwrap();
        }
        let overflow = add_idea(&per_key, "per-key-overflow");
        assert_eq!(
            create_for_idea(
                &per_key,
                overflow,
                "per-key-overflow",
                vec![ProgramRunLockDomainV1::DangerousMutation],
            )
            .unwrap_err(),
            ProgramRunStoreError::QueueBackpressure
        );

        let project = fixture("D05 project cap");
        for index in 0..1_024 {
            let idea_id = add_idea(&project, &format!("project-{index}"));
            create_for_idea(
                &project,
                idea_id,
                &format!("project-{index}"),
                vec![ProgramRunLockDomainV1::IdeaController],
            )
            .unwrap();
        }
        let overflow = add_idea(&project, "project-overflow");
        assert_eq!(
            create_for_idea(
                &project,
                overflow,
                "project-overflow",
                vec![ProgramRunLockDomainV1::IdeaController],
            )
            .unwrap_err(),
            ProgramRunStoreError::QueueBackpressure
        );
    }

    #[test]
    fn d05_six_budgets_reserve_consume_release_and_exhaust_independently() {
        let fixture = fixture("D05 budgets");
        let created = create(&fixture, "budgets-create", vec![]);
        let budgets = fixture
            .store
            .get_program_run_operational_status_v1(created.run.id, true, Utc::now())
            .unwrap()
            .budgets;
        assert_eq!(budgets.len(), 6);
        assert_eq!(
            budgets
                .iter()
                .map(|budget| budget.dimension)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            6
        );
        let _ready = ready(&fixture, &created.run, "budget-ready");
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot_id = fixture.store.program_run_boot_id();
        let claim = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        let running = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &claim_transition(&claim, "budget-claimed"),
            )
            .unwrap();
        let reserved = fixture
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap()
            .budgets;
        let work = reserved
            .iter()
            .find(|budget| budget.dimension == ProgramRunBudgetDimensionV1::WorkAttempts)
            .unwrap();
        assert_eq!(
            (work.reserved_value, work.used_value, work.row_version),
            (1, 0, 1)
        );
        fixture
            .store
            .record_program_run_publication_v1(
                &scheduler,
                claim.action.id,
                boot_id,
                claim.action.claim_generation,
                Utc::now(),
            )
            .unwrap();
        fixture
            .store
            .bind_program_run_external_reference_v1(
                &scheduler,
                claim.action.id,
                boot_id,
                claim.action.claim_generation,
                ProgramRunExternalReferenceV1::Session(fixture.controller_id),
                Utc::now(),
            )
            .unwrap();
        let consumed = fixture
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap()
            .budgets;
        let work = consumed
            .iter()
            .find(|budget| budget.dimension == ProgramRunBudgetDimensionV1::WorkAttempts)
            .unwrap();
        assert_eq!(
            (work.reserved_value, work.used_value, work.row_version),
            (0, 1, 2)
        );
        assert!(
            consumed
                .iter()
                .find(|budget| {
                    budget.dimension == ProgramRunBudgetDimensionV1::ProductiveTransitions
                })
                .unwrap()
                .row_version
                >= 4
        );
        let retry = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                &ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                    program_run_id: running.run.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    operation: ProgramRunOperationV1::RetryScheduled,
                    idempotency_key: "budget-retry".into(),
                    reason: Some("provider retry".into()),
                }),
            )
            .unwrap();
        let retry_budgets = fixture
            .store
            .get_program_run_operational_status_v1(retry.run.id, true, Utc::now())
            .unwrap()
            .budgets;
        for dimension in [
            ProgramRunBudgetDimensionV1::LaunchRetries,
            ProgramRunBudgetDimensionV1::WakeReservations,
        ] {
            let budget = retry_budgets
                .iter()
                .find(|budget| budget.dimension == dimension)
                .unwrap();
            assert_eq!(
                (budget.reserved_value, budget.used_value, budget.row_version),
                (0, 1, 2)
            );
        }

        let release_fixture = self::fixture("D05 budget release");
        let release_created = create(&release_fixture, "release-create", vec![]);
        let _release_ready = self::ready(&release_fixture, &release_created.run, "release-ready");
        let release_scheduler =
            ProgramRunStoreAuthority::scheduler(release_fixture.controller_id, 1);
        let release_boot = release_fixture.store.program_run_boot_id();
        let release_claim = release_fixture
            .store
            .claim_due_program_run_actions_v1(
                &release_scheduler,
                release_boot,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        let release_running = release_fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(release_fixture.controller_id, 1),
                &claim_transition(&release_claim, "release-claimed"),
            )
            .unwrap();
        release_fixture
            .store
            .acknowledge_program_run_action_v1(
                &release_scheduler,
                release_claim.action.id,
                release_boot,
                release_claim.action.claim_generation,
                Some(("provider_rejected", "confirmed before launch", true)),
                Utc::now(),
            )
            .unwrap();
        let released = release_fixture
            .store
            .get_program_run_operational_status_v1(release_running.run.id, true, Utc::now())
            .unwrap()
            .budgets;
        let work = released
            .iter()
            .find(|budget| budget.dimension == ProgramRunBudgetDimensionV1::WorkAttempts)
            .unwrap();
        assert_eq!(
            (work.reserved_value, work.used_value, work.row_version),
            (0, 0, 2)
        );
    }

    #[test]
    fn d05_each_budget_exhausts_at_its_production_semantic_boundary() {
        let one_productive = fixture("D05 productive exhaustion");
        let mut productive_template = template(vec![]);
        productive_template.budgets.productive_transitions = 1;
        let created = one_productive
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: one_productive.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "productive-create".into(),
                    template: productive_template,
                },
            )
            .unwrap();
        let _ready = self::ready(&one_productive, &created.run, "productive-ready");
        let scheduler = ProgramRunStoreAuthority::scheduler(one_productive.controller_id, 1);
        let boot_id = one_productive.store.program_run_boot_id();
        let productive_claim = one_productive
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            one_productive
                .store
                .apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::controller(one_productive.controller_id, 1),
                    &claim_transition(&productive_claim, "productive-exhausted"),
                )
                .unwrap_err(),
            ProgramRunStoreError::BudgetExhausted
        );

        let one_work = fixture("D05 work exhaustion");
        let mut work_template = template(vec![]);
        work_template.budgets.work_attempts = 1;
        work_template.cursors.push(ProgramRunCursorV1 {
            key: "verify".into(),
            phase: "verification".into(),
            required_gates: vec![],
            revision_target_ordinal: None,
        });
        let created = one_work
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: one_work.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "work-create".into(),
                    template: work_template,
                },
            )
            .unwrap();
        let ready = self::ready(&one_work, &created.run, "work-ready");
        let (running, attempt) = launch_current(&one_work, &ready.run, "work-first");
        let _next = one_work
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(one_work.controller_id, 1),
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: running.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    idempotency_key: "work-first-output".into(),
                    output_ref: "cas://work-first".into(),
                    output_digest: format!("sha256:{}", "6".repeat(64)),
                }),
            )
            .unwrap();
        let scheduler = ProgramRunStoreAuthority::scheduler(one_work.controller_id, 1);
        let exhausted_claim = one_work
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                one_work.store.program_run_boot_id(),
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            one_work
                .store
                .apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::controller(one_work.controller_id, 1),
                    &claim_transition(&exhausted_claim, "work-exhausted"),
                )
                .unwrap_err(),
            ProgramRunStoreError::BudgetExhausted
        );

        for (dimension, key) in [
            (ProgramRunBudgetDimensionV1::LaunchRetries, "launch"),
            (ProgramRunBudgetDimensionV1::WakeReservations, "wake"),
        ] {
            let fixture = fixture(&format!("D05 {key} exhaustion"));
            let mut retry_template = template(vec![]);
            if dimension == ProgramRunBudgetDimensionV1::LaunchRetries {
                retry_template.budgets.launch_retries = 1;
            } else {
                retry_template.budgets.wake_reservations = 1;
            }
            let created = fixture
                .store
                .create_program_run_v1(
                    &ProgramRunStoreAuthority::operator(),
                    &CreateProgramRunRequestV1 {
                        idea_id: fixture.idea_id,
                        expected_idea_row_version: 0,
                        idempotency_key: format!("{key}-create"),
                        template: retry_template,
                    },
                )
                .unwrap();
            let ready = self::ready(&fixture, &created.run, &format!("{key}-ready"));
            let (running, _) = launch_current(&fixture, &ready.run, &format!("{key}-first"));
            let retrying = fixture
                .store
                .apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                    &transition(
                        &running.run,
                        ProgramRunOperationV1::RetryScheduled,
                        &format!("{key}-retry-first"),
                    ),
                )
                .unwrap();
            let ready_again =
                acknowledge_retry_wake(&fixture, &retrying.run, &format!("{key}-wake-first"));
            let (running_again, _) =
                launch_current_in_new_session(&fixture, &ready_again.run, &format!("{key}-second"));
            assert_eq!(
                fixture
                    .store
                    .apply_program_run_transition_v1(
                        &ProgramRunStoreAuthority::controller(fixture.controller_id, 1),
                        &transition(
                            &running_again.run,
                            ProgramRunOperationV1::RetryScheduled,
                            &format!("{key}-exhausted"),
                        ),
                    )
                    .unwrap_err(),
                ProgramRunStoreError::BudgetExhausted
            );
            let budget = fixture
                .store
                .get_program_run_operational_status_v1(running_again.run.id, true, Utc::now())
                .unwrap()
                .budgets
                .into_iter()
                .find(|budget| budget.dimension == dimension)
                .unwrap();
            assert_eq!((budget.reserved_value, budget.used_value), (0, 1));
        }

        let revisions = fixture("D05 revision exhaustion");
        let mut revision_template = template(vec![]);
        revision_template.budgets.revisions = 1;
        revision_template.cursors[0].required_gates = vec![ProgramRunGateRequirementV1 {
            gate_key: "review".into(),
            policy_key: "review-v1".into(),
            policy_version: 1,
        }];
        revision_template.cursors[0].revision_target_ordinal = Some(0);
        let created = revisions
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: revisions.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "revision-create".into(),
                    template: revision_template,
                },
            )
            .unwrap();
        let ready = self::ready(&revisions, &created.run, "revision-ready");
        let (running, attempt) = launch_current(&revisions, &ready.run, "revision-first");
        let awaiting = revisions
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(revisions.controller_id, 1),
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: running.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    idempotency_key: "revision-output-first".into(),
                    output_ref: "cas://revision-first".into(),
                    output_digest: format!("sha256:{}", "7".repeat(64)),
                }),
            )
            .unwrap();
        let retrying = revisions
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(revisions.controller_id, 1),
                &ProgramRunTransitionInputV1::Gate(RecordProgramRunGateRequestV1 {
                    program_run_id: awaiting.run.id,
                    expected_run_version: awaiting.run.row_version,
                    expected_idea_version: awaiting.run.idea_row_version,
                    idempotency_key: "revision-gate-first".into(),
                    gate_key: "review".into(),
                    result: ProgramRunGateResultV1::Failed,
                    policy_key: "review-v1".into(),
                    policy_version: 1,
                    evidence_ref: "cas://revision-review-first".into(),
                    evidence_digest: format!("sha256:{}", "8".repeat(64)),
                }),
            )
            .unwrap();
        let ready_again = acknowledge_retry_wake(&revisions, &retrying.run, "revision-wake");
        let (running, attempt) =
            launch_current_in_new_session(&revisions, &ready_again.run, "revision-second");
        let awaiting = revisions
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(revisions.controller_id, 1),
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: running.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    idempotency_key: "revision-output-second".into(),
                    output_ref: "cas://revision-second".into(),
                    output_digest: format!("sha256:{}", "9".repeat(64)),
                }),
            )
            .unwrap();
        assert_eq!(
            revisions
                .store
                .apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::controller(revisions.controller_id, 1),
                    &ProgramRunTransitionInputV1::Gate(RecordProgramRunGateRequestV1 {
                        program_run_id: awaiting.run.id,
                        expected_run_version: awaiting.run.row_version,
                        expected_idea_version: awaiting.run.idea_row_version,
                        idempotency_key: "revision-exhausted".into(),
                        gate_key: "review".into(),
                        result: ProgramRunGateResultV1::Failed,
                        policy_key: "review-v1".into(),
                        policy_version: 1,
                        evidence_ref: "cas://revision-review-second".into(),
                        evidence_digest: format!("sha256:{}", "a".repeat(64)),
                    }),
                )
                .unwrap_err(),
            ProgramRunStoreError::BudgetExhausted
        );

        let publication = fixture("D05 publication retry exhaustion");
        let mut publication_template = template(vec![]);
        publication_template.budgets.action_publication_retries = 1;
        let created = publication
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: publication.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "publication-create".into(),
                    template: publication_template,
                },
            )
            .unwrap();
        let ready = self::ready(&publication, &created.run, "publication-ready");
        let scheduler = ProgramRunStoreAuthority::scheduler(publication.controller_id, 1);
        let boot_id = publication.store.program_run_boot_id();
        let first_time = Utc::now();
        publication
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                first_time,
                1,
            )
            .unwrap();
        let live = HashMap::from([(publication.controller_id, 1)]);
        let second_time = first_time + Duration::seconds(31);
        publication
            .store
            .reconcile_program_runs_page_v1(
                Some(publication.project_id),
                None,
                256,
                2_000,
                false,
                second_time,
                &live,
                boot_id,
            )
            .unwrap();
        publication
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                second_time,
                1,
            )
            .unwrap();
        let third_time = second_time + Duration::seconds(31);
        publication
            .store
            .reconcile_program_runs_page_v1(
                Some(publication.project_id),
                None,
                256,
                2_000,
                false,
                third_time,
                &live,
                boot_id,
            )
            .unwrap();
        assert!(
            publication
                .store
                .claim_due_program_run_actions_v1(
                    &scheduler,
                    boot_id,
                    &[ProgramRunActionKindV1::Work],
                    third_time,
                    1,
                )
                .unwrap()
                .is_empty(),
            "publication retry exhaustion settles locally without another publication"
        );
        assert_eq!(
            publication
                .store
                .get_program_run_v1(ready.run.id)
                .unwrap()
                .unwrap()
                .row_version,
            ready.run.row_version,
            "publication exhaustion must not mutate the run projection"
        );
    }

    #[test]
    fn d05_terminal_session_never_commits_output_gate_or_settlement() {
        let fixture = fixture("D05 terminal fact");
        let mut gated_template = template(vec![]);
        gated_template.cursors[0].required_gates = vec![ProgramRunGateRequirementV1 {
            gate_key: "verification".into(),
            policy_key: "d05-explicit-proof".into(),
            policy_version: 1,
        }];
        let created = fixture
            .store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: fixture.idea_id,
                    expected_idea_row_version: 0,
                    idempotency_key: "terminal-create".into(),
                    template: gated_template,
                },
            )
            .unwrap();
        let controller = ProgramRunStoreAuthority::controller(fixture.controller_id, 1);
        let _ready = fixture
            .store
            .apply_program_run_transition_v1(
                &controller,
                &transition(
                    &created.run,
                    ProgramRunOperationV1::LocksGranted,
                    "terminal-ready",
                ),
            )
            .unwrap();
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot_id = fixture.store.program_run_boot_id();
        let claim = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        let running = fixture
            .store
            .apply_program_run_transition_v1(
                &controller,
                &claim_transition(&claim, "terminal-attempt"),
            )
            .unwrap();
        fixture
            .store
            .record_program_run_publication_v1(
                &scheduler,
                claim.action.id,
                boot_id,
                claim.action.claim_generation,
                Utc::now(),
            )
            .unwrap();
        fixture
            .store
            .bind_program_run_external_reference_v1(
                &scheduler,
                claim.action.id,
                boot_id,
                claim.action.claim_generation,
                ProgramRunExternalReferenceV1::Session(fixture.controller_id),
                Utc::now(),
            )
            .unwrap();
        let attempt = fixture
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap()
            .current_attempt
            .unwrap();
        assert_eq!(attempt.state, ProgramRunAttemptStateV1::Launched);

        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [fixture.controller_id.to_string()],
            )
            .unwrap();
        let run = fixture
            .store
            .get_program_run_v1(running.run.id)
            .unwrap()
            .unwrap();
        assert_eq!(run.status, ProgramRunStatusV1::Running);
        assert!(run.settled_at.is_none());
        let gates: i64 = fixture
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM idea_program_run_gates WHERE program_run_id=?1",
                [run.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(gates, 0);

        let observed = fixture
            .store
            .apply_program_run_transition_v1(
                &controller,
                &ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                    program_run_id: run.id,
                    expected_run_version: run.row_version,
                    expected_idea_version: run.idea_row_version,
                    operation: ProgramRunOperationV1::AttemptTerminalObserved,
                    idempotency_key: "terminal-observed".into(),
                    reason: Some("Completed".into()),
                }),
            )
            .unwrap();
        assert_eq!(observed.run.status, ProgramRunStatusV1::Running);
        let awaiting_gate = fixture
            .store
            .apply_program_run_transition_v1(
                &controller,
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: observed.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: observed.run.row_version,
                    expected_idea_version: observed.run.idea_row_version,
                    idempotency_key: "explicit-output".into(),
                    output_ref: "cas://explicit-output".into(),
                    output_digest: format!("sha256:{}", "c".repeat(64)),
                }),
            )
            .unwrap();
        assert_eq!(awaiting_gate.run.status, ProgramRunStatusV1::AwaitingGate);
        let awaiting_status = fixture
            .store
            .get_program_run_operational_status_v1(awaiting_gate.run.id, true, Utc::now())
            .unwrap();
        assert_eq!(
            awaiting_status.idea_controller_session_id,
            Some(fixture.controller_id)
        );
        assert_eq!(awaiting_status.idea_controller_epoch, 1);
        assert!(awaiting_status.controller_matches_idea);
        assert_eq!(awaiting_status.required_gates.len(), 1);
        assert_eq!(awaiting_status.required_gates[0].gate_key, "verification");
        assert!(
            awaiting_status.required_gates[0]
                .latest_evaluation
                .is_none()
        );
        assert_eq!(
            awaiting_status.next_action,
            ProgramRunNextActionV1::EvaluateGate
        );
        assert_eq!(
            awaiting_status.active_action.as_ref().unwrap().purpose,
            ProgramRunActionPurposeV1::EvaluateGates
        );
        let settled = fixture
            .store
            .apply_program_run_transition_v1(
                &controller,
                &ProgramRunTransitionInputV1::Gate(RecordProgramRunGateRequestV1 {
                    program_run_id: awaiting_gate.run.id,
                    expected_run_version: awaiting_gate.run.row_version,
                    expected_idea_version: awaiting_gate.run.idea_row_version,
                    idempotency_key: "explicit-gate".into(),
                    gate_key: "verification".into(),
                    result: ProgramRunGateResultV1::Passed,
                    policy_key: "d05-explicit-proof".into(),
                    policy_version: 1,
                    evidence_ref: "cas://explicit-evidence".into(),
                    evidence_digest: format!("sha256:{}", "d".repeat(64)),
                }),
            )
            .unwrap();
        assert_eq!(settled.run.status, ProgramRunStatusV1::Settled);
    }

    #[test]
    fn d05_rr2_delayed_semantic_claim_is_fenced_without_mutation() {
        let fixture = fixture("D05 RR2 stale semantic claim");
        let created = create(&fixture, "rr2-claim-create", vec![]);
        let ready = ready(&fixture, &created.run, "rr2-claim-ready");
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot = fixture.store.program_run_boot_id();
        let first = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        let delayed = first.semantic_claim.clone().unwrap();
        fixture
            .store
            .conn
            .execute(
                "UPDATE idea_program_run_actions SET claim_expires_at=?1 WHERE id=?2",
                params![
                    timestamp(Utc::now() - Duration::seconds(1)),
                    first.action.id.to_string()
                ],
            )
            .unwrap();
        let mut live = HashMap::new();
        live.insert(fixture.controller_id, 1);
        fixture
            .store
            .reconcile_program_runs_page_v1(None, None, 8, 2_000, false, Utc::now(), &live, boot)
            .unwrap();
        let replacement = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        assert!(replacement.action.claim_generation > delayed.claim_generation);
        let observed_at = Utc::now();
        let before = fixture
            .store
            .get_program_run_operational_status_v1(ready.run.id, true, observed_at)
            .unwrap();
        assert_eq!(
            fixture
                .store
                .apply_program_run_transition_v1(
                    &scheduler,
                    &ProgramRunTransitionInputV1::ClaimAction(delayed),
                )
                .unwrap_err(),
            ProgramRunStoreError::StaleClaimGeneration,
        );
        assert_eq!(
            fixture
                .store
                .get_program_run_operational_status_v1(ready.run.id, true, observed_at,)
                .unwrap(),
            before
        );

        let boot_fixture = self::fixture("D05 RR2 stale boot semantic claim");
        let boot_created = create(&boot_fixture, "rr2-boot-create", vec![]);
        let boot_ready = self::ready(&boot_fixture, &boot_created.run, "rr2-boot-ready");
        let old_boot = boot_fixture.store.program_run_boot_id();
        let boot_claim = boot_fixture
            .store
            .claim_due_program_run_actions_v1(
                &ProgramRunStoreAuthority::scheduler(boot_fixture.controller_id, 1),
                old_boot,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap()
            .semantic_claim
            .unwrap();
        let boot_observed_at = Utc::now();
        let boot_before = boot_fixture
            .store
            .get_program_run_operational_status_v1(boot_ready.run.id, true, boot_observed_at)
            .unwrap();
        boot_fixture
            .store
            .set_program_run_boot_id(Uuid::new_v4())
            .unwrap();
        assert_eq!(
            boot_fixture
                .store
                .apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::scheduler(boot_fixture.controller_id, 1),
                    &ProgramRunTransitionInputV1::ClaimAction(boot_claim),
                )
                .unwrap_err(),
            ProgramRunStoreError::StaleClaimGeneration,
        );
        assert_eq!(
            boot_fixture
                .store
                .get_program_run_operational_status_v1(boot_ready.run.id, true, boot_observed_at,)
                .unwrap(),
            boot_before
        );
    }

    #[test]
    fn d05_rr2_confirmed_pre_effect_failure_advertises_executable_cancel_and_replays() {
        let fixture = fixture("D05 RR2 failed reserved attempt");
        let created = create(&fixture, "rr2-failure-create", vec![]);
        let _ready = ready(&fixture, &created.run, "rr2-failure-ready");
        let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let boot = fixture.store.program_run_boot_id();
        let claim = fixture
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        let running = fixture
            .store
            .apply_program_run_transition_v1(
                &scheduler,
                &claim_transition(&claim, "rr2-failure-claimed"),
            )
            .unwrap();
        let failed = fixture
            .store
            .acknowledge_program_run_action_v1(
                &scheduler,
                claim.action.id,
                boot,
                claim.action.claim_generation,
                Some(("provider_rejected", "confirmed before effect", true)),
                Utc::now(),
            )
            .unwrap();
        let status = fixture
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap();
        assert_eq!(
            status.next_action,
            ProgramRunNextActionV1::OperatorCancelOnly
        );
        assert_eq!(
            status.current_attempt.as_ref().unwrap().state,
            ProgramRunAttemptStateV1::Failed
        );
        let work = status
            .budgets
            .iter()
            .find(|value| value.dimension == ProgramRunBudgetDimensionV1::WorkAttempts)
            .unwrap();
        assert_eq!((work.reserved_value, work.used_value), (0, 0));
        let cancelled = fixture
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::operator(),
                &ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                    program_run_id: running.run.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    operation: ProgramRunOperationV1::OperatorCancelled,
                    idempotency_key: "rr2-failure-cancel".into(),
                    reason: Some("failed reserved attempt".into()),
                }),
            )
            .unwrap();
        assert_eq!(cancelled.run.status, ProgramRunStatusV1::Cancelled);
        let replay = fixture
            .store
            .acknowledge_program_run_action_v1(
                &scheduler,
                claim.action.id,
                boot,
                claim.action.claim_generation,
                Some(("provider_rejected", "confirmed before effect", true)),
                Utc::now(),
            )
            .unwrap();
        assert!(replay.deduplicated);
        assert_eq!(replay.action, failed.action);
        assert_eq!(
            fixture
                .store
                .acknowledge_program_run_action_v1(
                    &scheduler,
                    claim.action.id,
                    boot,
                    claim.action.claim_generation,
                    Some(("provider_rejected", "changed", true)),
                    Utc::now(),
                )
                .unwrap_err(),
            ProgramRunStoreError::DownstreamReplayConflict
        );
        assert_eq!(
            fixture
                .store
                .acknowledge_program_run_action_v1(
                    &scheduler,
                    claim.action.id,
                    boot,
                    claim.action.claim_generation,
                    Some(("provider_rejected", "confirmed before effect", false)),
                    Utc::now(),
                )
                .unwrap_err(),
            ProgramRunStoreError::DownstreamReplayConflict,
            "the refund classification is part of exact replay identity"
        );
    }

    #[test]
    fn d05_v4_failure_replay_survives_store_reopen_without_any_fact_write() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let fixture = fixture_with_store(Store::open(&db_path).unwrap(), "D05 V4 replay reopen");
        let created = create(&fixture, "v4-reopen-create", vec![]);
        let _ready = ready(&fixture, &created.run, "v4-reopen-ready");
        let authority = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
        let old_boot = fixture.store.program_run_boot_id();
        let claim = fixture
            .store
            .claim_due_program_run_actions_v1(
                &authority,
                old_boot,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        fixture
            .store
            .apply_program_run_transition_v1(
                &authority,
                &claim_transition(&claim, "v4-reopen-claimed"),
            )
            .unwrap();
        let failed = fixture
            .store
            .acknowledge_program_run_action_v1(
                &authority,
                claim.action.id,
                old_boot,
                claim.action.claim_generation,
                Some(("provider_rejected", "canonical failure", true)),
                Utc::now(),
            )
            .unwrap();
        let run_id = created.run.id;
        let idea_id = fixture.idea_id;
        let controller_id = fixture.controller_id;
        drop(fixture);

        let reopened = Store::open(&db_path).unwrap();
        let new_boot = reopened.program_run_boot_id();
        assert_ne!(new_boot, old_boot);
        let status_before = reopened
            .get_program_run_operational_status_v1(run_id, true, Utc::now())
            .unwrap();
        let idea_before = reopened.get_idea_with_genesis(idea_id).unwrap();
        let events_before: String = reopened
            .conn
            .query_row(
                "SELECT group_concat(id||':'||sequence||':'||event_type||':'||
                 hex(CAST(payload_json AS BLOB)),',') FROM idea_events WHERE idea_id=?1
                 ORDER BY sequence",
                [idea_id.to_string()],
                |row| Ok(row.get::<_, Option<String>>(0)?.unwrap_or_default()),
            )
            .unwrap();
        let bytes_before = std::fs::read(&db_path).unwrap();

        let replay = reopened
            .acknowledge_program_run_action_v1(
                &ProgramRunStoreAuthority::scheduler(controller_id, 1),
                claim.action.id,
                new_boot,
                claim.action.claim_generation,
                Some(("provider_rejected", "canonical failure", true)),
                Utc::now(),
            )
            .unwrap();
        assert!(replay.deduplicated);
        assert_eq!(replay.action, failed.action);
        assert_eq!(
            reopened
                .get_program_run_operational_status_v1(run_id, true, Utc::now())
                .unwrap(),
            status_before
        );
        assert_eq!(
            reopened.get_idea_with_genesis(idea_id).unwrap(),
            idea_before
        );
        let events_after: String = reopened
            .conn
            .query_row(
                "SELECT group_concat(id||':'||sequence||':'||event_type||':'||
                 hex(CAST(payload_json AS BLOB)),',') FROM idea_events WHERE idea_id=?1
                 ORDER BY sequence",
                [idea_id.to_string()],
                |row| Ok(row.get::<_, Option<String>>(0)?.unwrap_or_default()),
            )
            .unwrap();
        assert_eq!(events_after, events_before);
        assert_eq!(std::fs::read(&db_path).unwrap(), bytes_before);

        assert_eq!(
            reopened
                .acknowledge_program_run_action_v1(
                    &ProgramRunStoreAuthority::scheduler(controller_id, 2),
                    claim.action.id,
                    new_boot,
                    claim.action.claim_generation,
                    Some(("provider_rejected", "canonical failure", true)),
                    Utc::now(),
                )
                .unwrap_err(),
            ProgramRunStoreError::StaleControllerEpoch
        );
        assert_eq!(
            reopened
                .acknowledge_program_run_action_v1(
                    &ProgramRunStoreAuthority::scheduler(Uuid::new_v4(), 1),
                    claim.action.id,
                    new_boot,
                    claim.action.claim_generation,
                    Some(("provider_rejected", "canonical failure", true)),
                    Utc::now(),
                )
                .unwrap_err(),
            ProgramRunStoreError::ControllerMismatch
        );
        for failure in [
            ("opposite_class", "canonical failure", true),
            ("provider_rejected", "opposite message", true),
            ("provider_rejected", "canonical failure", false),
        ] {
            assert_eq!(
                reopened
                    .acknowledge_program_run_action_v1(
                        &authority,
                        claim.action.id,
                        new_boot,
                        claim.action.claim_generation,
                        Some(failure),
                        Utc::now(),
                    )
                    .unwrap_err(),
                ProgramRunStoreError::DownstreamReplayConflict
            );
        }
        assert_eq!(
            reopened
                .acknowledge_program_run_action_v1(
                    &authority,
                    claim.action.id,
                    new_boot,
                    claim.action.claim_generation,
                    None,
                    Utc::now(),
                )
                .unwrap_err(),
            ProgramRunStoreError::DownstreamReplayConflict
        );
        assert_eq!(
            reopened
                .acknowledge_program_run_action_v1(
                    &authority,
                    Uuid::new_v4(),
                    new_boot,
                    claim.action.claim_generation,
                    Some(("provider_rejected", "canonical failure", true)),
                    Utc::now(),
                )
                .unwrap_err(),
            ProgramRunStoreError::NotFound
        );
        assert_eq!(std::fs::read(&db_path).unwrap(), bytes_before);
    }

    #[test]
    fn d05_v4_failed_attempt_status_ignores_consumed_acknowledgement_history() {
        for (name, confirmed_pre_effect) in [("confirmed", true), ("uncertain", false)] {
            let fixture = fixture(&format!("D05 V4 historical {name}"));
            let ready = ready_run_with_consumed_acknowledgement_history(
                &fixture,
                &format!("v4-history-{name}"),
            );
            let scheduler = ProgramRunStoreAuthority::scheduler(fixture.controller_id, 1);
            let boot = fixture.store.program_run_boot_id();
            let claim = fixture
                .store
                .claim_due_program_run_actions_v1(
                    &scheduler,
                    boot,
                    &[ProgramRunActionKindV1::Work],
                    Utc::now(),
                    1,
                )
                .unwrap()
                .pop()
                .unwrap();
            let running = fixture
                .store
                .apply_program_run_transition_v1(
                    &scheduler,
                    &claim_transition(&claim, &format!("v4-{name}-claimed")),
                )
                .unwrap();
            fixture
                .store
                .acknowledge_program_run_action_v1(
                    &scheduler,
                    claim.action.id,
                    boot,
                    claim.action.claim_generation,
                    Some(("provider_failed", name, confirmed_pre_effect)),
                    Utc::now(),
                )
                .unwrap();
            let status = fixture
                .store
                .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
                .unwrap();
            assert!(status.active_action.is_none());
            assert_eq!(
                status.next_action,
                ProgramRunNextActionV1::OperatorCancelOnly
            );
            assert_eq!(
                status.current_attempt.as_ref().unwrap().state,
                ProgramRunAttemptStateV1::Failed
            );
            let cancelled = fixture
                .store
                .apply_program_run_transition_v1(
                    &ProgramRunStoreAuthority::operator(),
                    &ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                        program_run_id: running.run.id,
                        expected_run_version: running.run.row_version,
                        expected_idea_version: running.run.idea_row_version,
                        operation: ProgramRunOperationV1::OperatorCancelled,
                        idempotency_key: format!("v4-{name}-cancel"),
                        reason: Some(format!("{name} publication failure")),
                    }),
                )
                .unwrap();
            assert_eq!(cancelled.run.status, ProgramRunStatusV1::Cancelled);
            assert_ne!(ready.run.id, Uuid::nil());
        }

        let launched = fixture("D05 V4 historical launched failure");
        let ready =
            ready_run_with_consumed_acknowledgement_history(&launched, "v4-history-launched");
        let (running, attempt) = launch_current(&launched, &ready.run, "v4-launched");
        let before_failure = launched
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap();
        let action = before_failure.active_action.unwrap();
        launched
            .store
            .acknowledge_program_run_action_v1(
                &ProgramRunStoreAuthority::scheduler(launched.controller_id, 1),
                action.id,
                action.claim_boot_id.unwrap(),
                action.claim_generation,
                Some(("provider_failed", "post effect", true)),
                Utc::now(),
            )
            .unwrap();
        let status = launched
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap();
        assert!(status.active_action.is_none());
        assert_eq!(status.next_action, ProgramRunNextActionV1::CommitOutput);
        assert_eq!(
            status.current_attempt.as_ref().unwrap().state,
            ProgramRunAttemptStateV1::Launched
        );
        let awaiting = launched
            .store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::controller(launched.controller_id, 1),
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: running.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    idempotency_key: "v4-launched-output".into(),
                    output_ref: "cas://v4-launched-output".into(),
                    output_digest: format!("sha256:{}", "b".repeat(64)),
                }),
            )
            .unwrap();
        assert_eq!(awaiting.run.status, ProgramRunStatusV1::AwaitingGate);
    }

    #[test]
    fn d05_rr2_uncertain_and_post_effect_failures_never_refund_work_budget() {
        let uncertain = fixture("D05 RR2 uncertain failure");
        let created = create(&uncertain, "rr2-uncertain-create", vec![]);
        let _ready = ready(&uncertain, &created.run, "rr2-uncertain-ready");
        let scheduler = ProgramRunStoreAuthority::scheduler(uncertain.controller_id, 1);
        let boot = uncertain.store.program_run_boot_id();
        let claim = uncertain
            .store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot,
                &[ProgramRunActionKindV1::Work],
                Utc::now(),
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        let running = uncertain
            .store
            .apply_program_run_transition_v1(
                &scheduler,
                &claim_transition(&claim, "rr2-uncertain-claimed"),
            )
            .unwrap();
        uncertain
            .store
            .acknowledge_program_run_action_v1(
                &scheduler,
                claim.action.id,
                boot,
                claim.action.claim_generation,
                Some(("provider_uncertain", "effect cannot be excluded", false)),
                Utc::now(),
            )
            .unwrap();
        let uncertain_status = uncertain
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap();
        let uncertain_work = uncertain_status
            .budgets
            .iter()
            .find(|budget| budget.dimension == ProgramRunBudgetDimensionV1::WorkAttempts)
            .unwrap();
        assert_eq!(
            (uncertain_work.reserved_value, uncertain_work.used_value),
            (1, 0)
        );
        assert_eq!(
            uncertain_status.next_action,
            ProgramRunNextActionV1::OperatorCancelOnly
        );

        let post_effect = fixture("D05 RR2 post-effect failure");
        let created = create(&post_effect, "rr2-post-effect-create", vec![]);
        let ready = ready(&post_effect, &created.run, "rr2-post-effect-ready");
        let (running, _) = launch_current(&post_effect, &ready.run, "rr2-post-effect");
        let status = post_effect
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap();
        let action = status.active_action.unwrap();
        post_effect
            .store
            .acknowledge_program_run_action_v1(
                &ProgramRunStoreAuthority::scheduler(post_effect.controller_id, 1),
                action.id,
                action.claim_boot_id.unwrap(),
                action.claim_generation,
                Some(("provider_failed", "failure followed a durable launch", true)),
                Utc::now(),
            )
            .unwrap();
        let post_status = post_effect
            .store
            .get_program_run_operational_status_v1(running.run.id, true, Utc::now())
            .unwrap();
        let post_work = post_status
            .budgets
            .iter()
            .find(|budget| budget.dimension == ProgramRunBudgetDimensionV1::WorkAttempts)
            .unwrap();
        assert_eq!((post_work.reserved_value, post_work.used_value), (0, 1));
        assert_eq!(
            post_status.next_action,
            ProgramRunNextActionV1::CommitOutput
        );
    }
}
