//! Durable provider-capacity incident, attempt, and delivery receipts (V89).

use std::collections::HashSet;
use std::path::PathBuf;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::types::{
    NewIssue, Recurrence, ScheduleSpec, ScheduledJob, SessionProvider, WakeMode,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::Store;
use crate::error::{DaemonError, Result};
use crate::model_control::ModelAdmissionRequest;

pub(super) const V89_TABLE_SQL: &str = r#"
CREATE TABLE master_no_idle_capacity_incidents (
    incident_id TEXT PRIMARY KEY
      CHECK(length(incident_id)=36 AND incident_id=lower(incident_id)
        AND substr(incident_id,9,1)='-' AND substr(incident_id,14,1)='-'
        AND substr(incident_id,19,1)='-' AND substr(incident_id,24,1)='-'
        AND replace(incident_id,'-','') NOT GLOB '*[^0-9a-f]*'),
    program_guard_job_id TEXT NOT NULL
      CHECK(length(program_guard_job_id)=36 AND program_guard_job_id=lower(program_guard_job_id)
        AND substr(program_guard_job_id,9,1)='-' AND substr(program_guard_job_id,14,1)='-'
        AND substr(program_guard_job_id,19,1)='-' AND substr(program_guard_job_id,24,1)='-'
        AND replace(program_guard_job_id,'-','') NOT GLOB '*[^0-9a-f]*')
      REFERENCES scheduled_jobs(id) ON DELETE RESTRICT,
    controller_session_id TEXT NOT NULL
      CHECK(length(controller_session_id)=36 AND controller_session_id=lower(controller_session_id)
        AND substr(controller_session_id,9,1)='-' AND substr(controller_session_id,14,1)='-'
        AND substr(controller_session_id,19,1)='-' AND substr(controller_session_id,24,1)='-'
        AND replace(controller_session_id,'-','') NOT GLOB '*[^0-9a-f]*')
      REFERENCES sessions(id) ON DELETE RESTRICT,
    capacity_class TEXT NOT NULL CHECK(capacity_class='codex_usage_limit'),
    outage_epoch INTEGER NOT NULL CHECK(outage_epoch>=1),
    state TEXT NOT NULL CHECK(state IN ('open','closed_success','closed_terminal')),
    backoff_bucket INTEGER NOT NULL CHECK(backoff_bucket BETWEEN 1 AND 7),
    wake_job_id TEXT NOT NULL UNIQUE
      CHECK(length(wake_job_id)=36 AND wake_job_id=lower(wake_job_id)
        AND substr(wake_job_id,9,1)='-' AND substr(wake_job_id,14,1)='-'
        AND substr(wake_job_id,19,1)='-' AND substr(wake_job_id,24,1)='-'
        AND replace(wake_job_id,'-','') NOT GLOB '*[^0-9a-f]*')
      REFERENCES scheduled_jobs(id) ON DELETE RESTRICT,
    issue_id TEXT UNIQUE CHECK(issue_id IS NULL OR
      (length(issue_id)=36 AND issue_id=lower(issue_id)
        AND substr(issue_id,9,1)='-' AND substr(issue_id,14,1)='-'
        AND substr(issue_id,19,1)='-' AND substr(issue_id,24,1)='-'
        AND replace(issue_id,'-','') NOT GLOB '*[^0-9a-f]*')),
    project_id TEXT CHECK(project_id IS NULL OR
      (length(project_id)=36 AND project_id=lower(project_id)
        AND substr(project_id,9,1)='-' AND substr(project_id,14,1)='-'
        AND substr(project_id,19,1)='-' AND substr(project_id,24,1)='-'
        AND replace(project_id,'-','') NOT GLOB '*[^0-9a-f]*'))
      REFERENCES projects(id) ON DELETE RESTRICT,
    provider TEXT NOT NULL CHECK(provider='Codex'),
    model TEXT,
    working_dir TEXT NOT NULL CHECK(length(working_dir)>0),
    last_capacity_model_invocation_id TEXT NOT NULL
      CHECK(length(last_capacity_model_invocation_id)=36
        AND last_capacity_model_invocation_id=lower(last_capacity_model_invocation_id)
        AND substr(last_capacity_model_invocation_id,9,1)='-'
        AND substr(last_capacity_model_invocation_id,14,1)='-'
        AND substr(last_capacity_model_invocation_id,19,1)='-'
        AND substr(last_capacity_model_invocation_id,24,1)='-'
        AND replace(last_capacity_model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*')
      REFERENCES model_invocations(id) ON DELETE RESTRICT,
    last_terminal_sequence INTEGER NOT NULL CHECK(last_terminal_sequence>=0),
    next_due_slot TEXT NOT NULL CHECK(next_due_slot GLOB '????-??-??T??:??:??.?????????Z'),
    opened_at TEXT NOT NULL CHECK(opened_at GLOB '????-??-??T??:??:??.?????????Z'),
    updated_at TEXT NOT NULL CHECK(updated_at GLOB '????-??-??T??:??:??.?????????Z'),
    closed_at TEXT CHECK(closed_at IS NULL OR closed_at GLOB '????-??-??T??:??:??.?????????Z'),
    close_reason TEXT CHECK(close_reason IS NULL OR close_reason IN ('non_capacity_success','program_terminal')),
    UNIQUE(program_guard_job_id,capacity_class,outage_epoch),
    FOREIGN KEY(issue_id,project_id) REFERENCES issues(id,project_id) ON DELETE RESTRICT,
    CHECK((issue_id IS NULL)=(project_id IS NULL)),
    CHECK(
      (state='open' AND closed_at IS NULL AND close_reason IS NULL)
      OR (state='closed_success' AND closed_at IS NOT NULL AND close_reason='non_capacity_success')
      OR (state='closed_terminal' AND closed_at IS NOT NULL AND close_reason='program_terminal')
    )
);

CREATE TABLE master_no_idle_capacity_attempts (
    incident_id TEXT NOT NULL
      CHECK(length(incident_id)=36 AND incident_id=lower(incident_id)
        AND substr(incident_id,9,1)='-' AND substr(incident_id,14,1)='-'
        AND substr(incident_id,19,1)='-' AND substr(incident_id,24,1)='-'
        AND replace(incident_id,'-','') NOT GLOB '*[^0-9a-f]*')
      REFERENCES master_no_idle_capacity_incidents(incident_id) ON DELETE RESTRICT,
    model_invocation_id TEXT NOT NULL UNIQUE
      CHECK(length(model_invocation_id)=36 AND model_invocation_id=lower(model_invocation_id)
        AND substr(model_invocation_id,9,1)='-' AND substr(model_invocation_id,14,1)='-'
        AND substr(model_invocation_id,19,1)='-' AND substr(model_invocation_id,24,1)='-'
        AND replace(model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*')
      REFERENCES model_invocations(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK(state IN ('delivery_admitted','delivery_launch_confirmed','capacity_failed','non_capacity_succeeded','non_capacity_failed','program_terminal')),
    resume_target_session_id TEXT
      CHECK(resume_target_session_id IS NULL OR
        (length(resume_target_session_id)=36 AND resume_target_session_id=lower(resume_target_session_id)
          AND substr(resume_target_session_id,9,1)='-' AND substr(resume_target_session_id,14,1)='-'
          AND substr(resume_target_session_id,19,1)='-' AND substr(resume_target_session_id,24,1)='-'
          AND replace(resume_target_session_id,'-','') NOT GLOB '*[^0-9a-f]*'))
      REFERENCES sessions(id) ON DELETE RESTRICT,
    delivery_wake_job_id TEXT
      CHECK(delivery_wake_job_id IS NULL OR
        (length(delivery_wake_job_id)=36 AND delivery_wake_job_id=lower(delivery_wake_job_id)
          AND substr(delivery_wake_job_id,9,1)='-' AND substr(delivery_wake_job_id,14,1)='-'
          AND substr(delivery_wake_job_id,19,1)='-' AND substr(delivery_wake_job_id,24,1)='-'
          AND replace(delivery_wake_job_id,'-','') NOT GLOB '*[^0-9a-f]*'))
      REFERENCES scheduled_jobs(id) ON DELETE RESTRICT,
    delivery_due_slot TEXT CHECK(delivery_due_slot IS NULL OR delivery_due_slot GLOB '????-??-??T??:??:??.?????????Z'),
    delivery_admitted_at TEXT CHECK(delivery_admitted_at IS NULL OR delivery_admitted_at GLOB '????-??-??T??:??:??.?????????Z'),
    delivery_launch_confirmed_at TEXT CHECK(delivery_launch_confirmed_at IS NULL OR delivery_launch_confirmed_at GLOB '????-??-??T??:??:??.?????????Z'),
    terminal_sequence INTEGER CHECK(terminal_sequence IS NULL OR terminal_sequence>=0),
    terminal_recorded_at TEXT CHECK(terminal_recorded_at IS NULL OR terminal_recorded_at GLOB '????-??-??T??:??:??.?????????Z'),
    created_at TEXT NOT NULL CHECK(created_at GLOB '????-??-??T??:??:??.?????????Z'),
    updated_at TEXT NOT NULL CHECK(updated_at GLOB '????-??-??T??:??:??.?????????Z'),
    PRIMARY KEY(incident_id,model_invocation_id),
    CHECK(
      (resume_target_session_id IS NULL AND delivery_wake_job_id IS NULL AND delivery_due_slot IS NULL AND delivery_admitted_at IS NULL)
      OR (resume_target_session_id IS NOT NULL AND delivery_wake_job_id IS NOT NULL AND delivery_due_slot IS NOT NULL AND delivery_admitted_at IS NOT NULL)
    ),
    CHECK(
      (state='delivery_admitted' AND resume_target_session_id IS NOT NULL
        AND delivery_launch_confirmed_at IS NULL
        AND terminal_sequence IS NULL AND terminal_recorded_at IS NULL)
      OR (state='delivery_launch_confirmed' AND resume_target_session_id IS NOT NULL
        AND delivery_launch_confirmed_at IS NOT NULL
        AND terminal_sequence IS NULL AND terminal_recorded_at IS NULL)
      OR (state IN ('capacity_failed','non_capacity_succeeded','non_capacity_failed','program_terminal')
        AND terminal_sequence IS NOT NULL AND terminal_recorded_at IS NOT NULL
        AND ((resume_target_session_id IS NULL AND delivery_launch_confirmed_at IS NULL)
          OR (resume_target_session_id IS NOT NULL AND delivery_launch_confirmed_at IS NOT NULL)))
    )
);
"#;

pub(super) const V89_INDEX_SQL: &str = r#"
CREATE UNIQUE INDEX idx_master_no_idle_capacity_incidents_open
  ON master_no_idle_capacity_incidents(program_guard_job_id,capacity_class)
  WHERE state='open';
CREATE INDEX idx_master_no_idle_capacity_incidents_controller
  ON master_no_idle_capacity_incidents(controller_session_id,state,opened_at DESC,incident_id DESC);
CREATE UNIQUE INDEX idx_master_no_idle_capacity_attempt_due
  ON master_no_idle_capacity_attempts(delivery_wake_job_id,delivery_due_slot)
  WHERE delivery_wake_job_id IS NOT NULL;
CREATE INDEX idx_master_no_idle_capacity_attempt_incident
  ON master_no_idle_capacity_attempts(incident_id,terminal_recorded_at,model_invocation_id);
"#;

pub(super) const V89_TRIGGER_SQL: &str = r#"
CREATE TRIGGER master_no_idle_capacity_incidents_no_delete
BEFORE DELETE ON master_no_idle_capacity_incidents
BEGIN SELECT RAISE(ABORT,'capacity incidents are immutable audit evidence'); END;

CREATE TRIGGER master_no_idle_capacity_attempts_no_delete
BEFORE DELETE ON master_no_idle_capacity_attempts
BEGIN SELECT RAISE(ABORT,'capacity attempts are immutable audit evidence'); END;

CREATE TRIGGER master_no_idle_capacity_incidents_identity_immutable
BEFORE UPDATE ON master_no_idle_capacity_incidents
WHEN NEW.incident_id!=OLD.incident_id
 OR NEW.program_guard_job_id!=OLD.program_guard_job_id
 OR NEW.controller_session_id!=OLD.controller_session_id
 OR NEW.capacity_class!=OLD.capacity_class OR NEW.outage_epoch!=OLD.outage_epoch
 OR NEW.wake_job_id!=OLD.wake_job_id OR NEW.issue_id IS NOT OLD.issue_id
 OR NEW.project_id IS NOT OLD.project_id OR NEW.provider!=OLD.provider
 OR NEW.model IS NOT OLD.model OR NEW.working_dir!=OLD.working_dir
 OR NEW.opened_at!=OLD.opened_at
BEGIN SELECT RAISE(ABORT,'capacity incident identity is immutable'); END;

CREATE TRIGGER master_no_idle_capacity_incidents_forward_state
BEFORE UPDATE ON master_no_idle_capacity_incidents
WHEN OLD.state!='open'
 OR NEW.state NOT IN ('open','closed_success','closed_terminal')
 OR NEW.backoff_bucket<OLD.backoff_bucket OR NEW.backoff_bucket>7
 OR (NEW.state='open' AND (NEW.closed_at IS NOT NULL OR NEW.close_reason IS NOT NULL))
 OR (NEW.state='closed_success' AND (NEW.closed_at IS NULL OR NEW.close_reason!='non_capacity_success'))
 OR (NEW.state='closed_terminal' AND (NEW.closed_at IS NULL OR NEW.close_reason!='program_terminal'))
BEGIN SELECT RAISE(ABORT,'invalid capacity incident transition'); END;

CREATE TRIGGER master_no_idle_capacity_attempts_identity_immutable
BEFORE UPDATE ON master_no_idle_capacity_attempts
WHEN NEW.incident_id!=OLD.incident_id OR NEW.model_invocation_id!=OLD.model_invocation_id
 OR NEW.resume_target_session_id IS NOT OLD.resume_target_session_id
 OR NEW.delivery_wake_job_id IS NOT OLD.delivery_wake_job_id
 OR NEW.delivery_due_slot IS NOT OLD.delivery_due_slot
 OR NEW.delivery_admitted_at IS NOT OLD.delivery_admitted_at
 OR NEW.created_at!=OLD.created_at
BEGIN SELECT RAISE(ABORT,'capacity attempt identity is immutable'); END;

CREATE TRIGGER master_no_idle_capacity_attempts_terminal_once
BEFORE UPDATE ON master_no_idle_capacity_attempts
WHEN NOT (
  (OLD.state='delivery_admitted'
    AND NEW.state='delivery_launch_confirmed'
    AND OLD.delivery_launch_confirmed_at IS NULL
    AND NEW.delivery_launch_confirmed_at IS NOT NULL
    AND NEW.terminal_sequence IS NULL AND NEW.terminal_recorded_at IS NULL)
  OR
  (OLD.state='delivery_launch_confirmed'
    AND NEW.state IN ('capacity_failed','non_capacity_succeeded','non_capacity_failed','program_terminal')
    AND NEW.delivery_launch_confirmed_at=OLD.delivery_launch_confirmed_at
    AND NEW.terminal_sequence IS NOT NULL AND NEW.terminal_recorded_at IS NOT NULL)
)
BEGIN SELECT RAISE(ABORT,'invalid capacity attempt terminal transition'); END;
"#;

const CAPACITY_CLASS: &str = "codex_usage_limit";
const INCIDENT_NAMESPACE: Uuid = Uuid::from_u128(0x0e84c043_5df7_5a10_87cb_093ace572b28);
const WAKE_NAMESPACE: Uuid = Uuid::from_u128(0xc1f552a1_9147_5066_8346_f10fb57cc7bd);
const ISSUE_NAMESPACE: Uuid = Uuid::from_u128(0x05a83884_5361_5685_a2be_5a0e11586133);
pub(crate) const BACKOFF_SECONDS: [i64; 7] = [60, 120, 240, 480, 960, 1920, 3600];

// Derived from the complete ordered sqlite_master catalog produced by exact
// accepted V88 source b9495e185e1f318745f3e3f51b9c46d36a2f858d.
pub(super) const V88_ACCEPTED_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:beb19a84bd9c98d71c1e5f6b16d9581e4b9adfc093e009b0335496a41b93ba99";

// Exact V88 catalog reached by the supported pre-provider V0 database shape
// exercised since fdd58214b. V6 appends `sessions.provider`, so this deployed
// predecessor has a different, but deterministic, column order from a fresh
// V88 database. Keep the finite pin instead of weakening the full-catalog gate.
pub(super) const V88_ACCEPTED_LEGACY_V0_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:976e0f6e88b4c2015eb19a8085680dea170e345da902fcefac105b6a5c0811ce";

// Exact deployed V88 catalog reached when the legacy-V0 lineage also retains
// two historical additive-schema residues: V27 appended
// `workflows.definition_json` to the V15 table (ab16eb775 -> c4dd25b10), and
// V55 created `recursive_live_attempts` before the non-migrating Antigravity
// CHECK update (66808c1d8 -> cf597a6e9). An object-by-object audit found those
// two table SQL rows to be the only differences from the accepted legacy-V0
// catalog. The regression fixture replays both transitions with SQLite DDL and
// must reproduce this full-catalog pin before V89 admission.
pub(super) const V88_ACCEPTED_DEPLOYED_ADDITIVE_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:87073def2a5f9605818cb85c127c1c8f9510bcfe57cdb856f9c805578f0879e0";

// Exact V88 catalog observed before Antigravity was added to the
// recursive_live_attempts provider CHECK. Its one-row historical DDL residue
// is otherwise identical to the accepted current predecessor above.
pub(super) const V88_ACCEPTED_PRE_ANTIGRAVITY_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:90094b8e6c3e1913beae85a159cfc18e3bde8df77b5a3642d3059729367eab67";

const V88_ACCEPTED_FULL_CATALOG_FINGERPRINTS: [&str; 4] = [
    V88_ACCEPTED_FULL_CATALOG_FINGERPRINT,
    V88_ACCEPTED_LEGACY_V0_FULL_CATALOG_FINGERPRINT,
    V88_ACCEPTED_DEPLOYED_ADDITIVE_FULL_CATALOG_FINGERPRINT,
    V88_ACCEPTED_PRE_ANTIGRAVITY_FULL_CATALOG_FINGERPRINT,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapacityAttemptContext {
    pub incident_id: Uuid,
    pub program_guard_job_id: Uuid,
    pub controller_session_id: Uuid,
    pub wake_job_id: Uuid,
    pub due_slot: DateTime<Utc>,
    pub attempt_state: String,
    pub incident_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapacityIssueDisposition {
    Attributed { issue_id: Uuid },
    Projectless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityCommitKind {
    New,
    Replay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityC5Resolution {
    ResolvedExact,
    AlreadyAbsent,
    Unrelated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapacityFailureSettlement {
    pub commit_kind: CapacityCommitKind,
    pub incident_id: Uuid,
    pub wake_job_id: Uuid,
    pub outage_epoch: i64,
    pub backoff_bucket: i64,
    pub due_slot: DateTime<Utc>,
    pub issue_disposition: CapacityIssueDisposition,
    pub c5_resolution: CapacityC5Resolution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapacityTerminalSettlement {
    pub commit_kind: CapacityCommitKind,
    pub incident_id: Uuid,
    pub wake_job_id: Uuid,
    pub issue_disposition: CapacityIssueDisposition,
    pub c5_resolution: CapacityC5Resolution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityCloseKind {
    NonCapacitySuccess,
    ProgramTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityCloseOutcome {
    Closed { incident_id: Uuid },
    NoOpenIncident,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityDeliveryPhase {
    Admitted,
    LaunchConfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapacityDeliveryReceipt {
    pub invocation_id: Uuid,
    pub phase: CapacityDeliveryPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityLaunchConfirmation {
    Committed,
    AlreadyConfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityC5Ownership {
    Owned(CapacityC5Resolution),
    NotOwned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapacityDeliveryPlan {
    pub incident_id: Uuid,
    pub controller_session_id: Uuid,
    pub wake_job_id: Uuid,
    pub due_slot: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapacityDeliveryValidation {
    NotCapacity,
    NotDue,
    StaleSnapshot,
    Ready(CapacityDeliveryPlan),
}

#[derive(Debug, Clone)]
pub(crate) struct CapacityAdmissionContext {
    pub incident_id: Uuid,
    pub target_session_id: Uuid,
    pub wake_job_id: Uuid,
    pub due_slot: String,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapacitySettlementFault {
    AfterWake,
    AfterIssue,
    AfterIncident,
    AfterAttempt,
    BeforeCommit,
}

#[cfg(test)]
thread_local! {
    static SETTLEMENT_FAULT: std::cell::RefCell<Option<CapacitySettlementFault>> = const { std::cell::RefCell::new(None) };
    static ADMISSION_FAULT: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
static LAUNCH_CONFIRMATION_FAULT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn test_fail_next_settlement(fault: CapacitySettlementFault) {
    SETTLEMENT_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
pub(crate) fn test_fail_next_admission(seam: &'static str) {
    ADMISSION_FAULT.with(|slot| *slot.borrow_mut() = Some(seam));
}

#[cfg(test)]
pub(crate) fn test_fail_next_launch_confirmation() {
    LAUNCH_CONFIRMATION_FAULT.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
fn settlement_fault(fault: CapacitySettlementFault) -> Result<()> {
    let injected = SETTLEMENT_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        Err(DaemonError::Store(format!(
            "capacity_recovery_transient:injected_{fault:?}"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn admission_fault(seam: &'static str) -> Result<()> {
    let injected = ADMISSION_FAULT.with(|slot| {
        if *slot.borrow() == Some(seam) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        Err(DaemonError::Store(format!(
            "capacity_admission_transient:injected_{seam}"
        )))
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
pub(crate) fn admission_fault(_: &'static str) -> Result<()> {
    Ok(())
}

fn nanos(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_uuid(label: &str, value: String) -> Result<Uuid> {
    let parsed = Uuid::parse_str(&value)
        .map_err(|error| DaemonError::Store(format!("invalid {label} UUID: {error}")))?;
    if parsed.to_string() != value {
        return Err(DaemonError::Store(format!(
            "non-canonical {label} UUID: {value}"
        )));
    }
    Ok(parsed)
}

fn parse_nanos(label: &str, value: String) -> Result<DateTime<Utc>> {
    let parsed = super::parse_timestamp(&value).map_err(DaemonError::Store)?;
    if nanos(parsed) != value {
        return Err(DaemonError::Store(format!(
            "non-canonical {label} timestamp: {value}"
        )));
    }
    Ok(parsed)
}

pub(super) fn v88_full_catalog_fingerprint(tx: &Transaction<'_>) -> Result<String> {
    let mut statement = tx.prepare(
        "SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master
         ORDER BY type,name,tbl_name,coalesce(sql,'')",
    )?;
    let mut rows = statement.query([])?;
    let mut digest = Sha256::new();
    while let Some(row) = rows.next()? {
        for index in 0..4 {
            let field: String = row.get(index)?;
            digest.update(field.as_bytes());
            if index != 3 {
                digest.update(b"\0");
            }
        }
        digest.update(b"\n");
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

pub(super) fn validate_v88_source(tx: &Transaction<'_>) -> Result<()> {
    let actual = v88_full_catalog_fingerprint(tx)?;
    if !V88_ACCEPTED_FULL_CATALOG_FINGERPRINTS.contains(&actual.as_str()) {
        return Err(DaemonError::Store(format!(
            "V89 requires exact V88 source catalog; expected one of {}, {}, {}, {}, got {actual}",
            V88_ACCEPTED_FULL_CATALOG_FINGERPRINT,
            V88_ACCEPTED_LEGACY_V0_FULL_CATALOG_FINGERPRINT,
            V88_ACCEPTED_DEPLOYED_ADDITIVE_FULL_CATALOG_FINGERPRINT,
            V88_ACCEPTED_PRE_ANTIGRAVITY_FULL_CATALOG_FINGERPRINT,
        )));
    }
    Ok(())
}

fn incident_id(program_guard_job_id: Uuid, epoch: i64) -> Uuid {
    Uuid::new_v5(
        &INCIDENT_NAMESPACE,
        format!("{program_guard_job_id}\0{CAPACITY_CLASS}\0{epoch}").as_bytes(),
    )
}

fn wake_id(incident_id: Uuid) -> Uuid {
    Uuid::new_v5(&WAKE_NAMESPACE, incident_id.as_bytes())
}

fn issue_id(incident_id: Uuid) -> Uuid {
    Uuid::new_v5(&ISSUE_NAMESPACE, incident_id.as_bytes())
}

fn lineage_contains_tx(tx: &Transaction<'_>, target: Uuid, controller: Uuid) -> Result<bool> {
    let mut current = target;
    let mut seen = HashSet::new();
    loop {
        if current == controller {
            return Ok(true);
        }
        if !seen.insert(current) {
            return Err(DaemonError::Store("capacity_resume_lineage_cycle".into()));
        }
        let parent: Option<Option<String>> = tx
            .query_row(
                "SELECT continued_from FROM sessions WHERE id=?1",
                [current.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(parent) = parent else {
            return Err(DaemonError::Store(format!(
                "capacity_resume_lineage_missing_session:{current}"
            )));
        };
        let Some(parent) = parent else {
            return Ok(false);
        };
        current = parse_uuid("continued_from", parent)?;
    }
}

fn collect_continued_from_ancestry_tx(tx: &Transaction<'_>, target: Uuid) -> Result<Vec<Uuid>> {
    let mut current = target;
    let mut seen = HashSet::new();
    let mut ancestry = Vec::new();
    loop {
        if !seen.insert(current) {
            return Err(DaemonError::Store("capacity_close_lineage_cycle".into()));
        }
        ancestry.push(current);
        let parent: Option<Option<String>> = tx
            .query_row(
                "SELECT continued_from FROM sessions WHERE id=?1",
                [current.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(parent) = parent else {
            return Err(DaemonError::Store(format!(
                "capacity_close_lineage_missing_session:{current}"
            )));
        };
        let Some(parent) = parent else {
            return Ok(ancestry);
        };
        current = parse_uuid("continued_from", parent)?;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CloseLookupStats {
    pub ancestry_nodes: usize,
    pub controller_index_lookups: usize,
}

type CloseIncidentIdentity = (Uuid, Uuid, Uuid);

fn select_open_incident_for_close_tx(
    tx: &Transaction<'_>,
    source_session_id: Uuid,
    receipt_context: Option<&CapacityAttemptContext>,
) -> Result<(Option<CloseIncidentIdentity>, CloseLookupStats)> {
    let ancestry = collect_continued_from_ancestry_tx(tx, source_session_id)?;
    if let Some(context) = receipt_context {
        if !ancestry.contains(&context.controller_session_id) {
            return Err(DaemonError::Store(
                "capacity close receipt controller lineage mismatch".into(),
            ));
        }
        return Ok((
            (context.incident_state == "open").then_some((
                context.incident_id,
                context.wake_job_id,
                context.program_guard_job_id,
            )),
            CloseLookupStats {
                ancestry_nodes: ancestry.len(),
                controller_index_lookups: 0,
            },
        ));
    }

    let mut newest: Option<(String, String, Uuid, Uuid, Uuid)> = None;
    for controller in &ancestry {
        let row: Option<(String, String, String, String)> = tx
            .query_row(
                "SELECT incident_id,opened_at,wake_job_id,program_guard_job_id
                 FROM master_no_idle_capacity_incidents
                 WHERE controller_session_id=?1 AND state='open'
                 ORDER BY opened_at DESC,incident_id DESC LIMIT 1",
                [controller.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((incident, opened_at, wake, guard)) = row {
            let parsed = (
                opened_at,
                incident.clone(),
                parse_uuid("incident", incident)?,
                parse_uuid("capacity wake", wake)?,
                parse_uuid("program guard", guard)?,
            );
            if newest
                .as_ref()
                .is_none_or(|prior| (&parsed.0, &parsed.1) > (&prior.0, &prior.1))
            {
                newest = Some(parsed);
            }
        }
    }
    Ok((
        newest.map(|(_, _, incident, wake, guard)| (incident, wake, guard)),
        CloseLookupStats {
            ancestry_nodes: ancestry.len(),
            controller_index_lookups: ancestry.len(),
        },
    ))
}

fn load_attempt_context_tx(
    tx: &Transaction<'_>,
    model_invocation_id: Uuid,
) -> Result<Option<CapacityAttemptContext>> {
    let raw = tx
        .query_row(
            "SELECT i.incident_id,i.program_guard_job_id,i.controller_session_id,
                    i.wake_job_id,i.next_due_slot,a.state,i.state
             FROM master_no_idle_capacity_attempts a
             JOIN master_no_idle_capacity_incidents i ON i.incident_id=a.incident_id
             WHERE a.model_invocation_id=?1",
            [model_invocation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |(incident, guard, controller, wake, due, attempt_state, incident_state)| {
            Ok(CapacityAttemptContext {
                incident_id: parse_uuid("incident", incident)?,
                program_guard_job_id: parse_uuid("program guard", guard)?,
                controller_session_id: parse_uuid("controller Session", controller)?,
                wake_job_id: parse_uuid("capacity wake", wake)?,
                due_slot: parse_nanos("capacity due slot", due)?,
                attempt_state,
                incident_state,
            })
        },
    )
    .transpose()
}

fn issue_disposition(issue_id: Option<Uuid>) -> CapacityIssueDisposition {
    issue_id.map_or(CapacityIssueDisposition::Projectless, |issue_id| {
        CapacityIssueDisposition::Attributed { issue_id }
    })
}

fn failure_settlement_tx(
    tx: &Transaction<'_>,
    incident: Uuid,
    commit_kind: CapacityCommitKind,
    c5_resolution: CapacityC5Resolution,
) -> Result<CapacityFailureSettlement> {
    let raw = tx.query_row(
        "SELECT wake_job_id,issue_id,outage_epoch,backoff_bucket,next_due_slot
         FROM master_no_idle_capacity_incidents WHERE incident_id=?1",
        [incident.to_string()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    )?;
    let parsed_issue = raw
        .1
        .map(|value| parse_uuid("capacity issue", value))
        .transpose()?;
    Ok(CapacityFailureSettlement {
        commit_kind,
        incident_id: incident,
        wake_job_id: parse_uuid("capacity wake", raw.0)?,
        outage_epoch: raw.2,
        backoff_bucket: raw.3,
        due_slot: parse_nanos("capacity due slot", raw.4)?,
        issue_disposition: issue_disposition(parsed_issue),
        c5_resolution,
    })
}

fn resolve_c5_if_capacity_owned_tx(
    tx: &Transaction<'_>,
    source_session_id: Uuid,
    pending_key: &str,
) -> Result<CapacityC5Resolution> {
    if super::daemon_settings::source_session_id_from_c5_pending_key(pending_key)
        != Some(source_session_id)
    {
        return Ok(CapacityC5Resolution::Unrelated);
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT value FROM daemon_settings WHERE key=?1",
            [pending_key],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(CapacityC5Resolution::AlreadyAbsent);
    };
    let Ok(marker) = super::daemon_settings::C5AutofilePending::parse(&raw) else {
        return Ok(CapacityC5Resolution::Unrelated);
    };
    if marker.source_session_id != source_session_id {
        return Ok(CapacityC5Resolution::Unrelated);
    }
    let invocation_id = if marker.version == 2 {
        marker.terminal_model_invocation_id
    } else {
        tx.query_row(
            "SELECT model_invocation_id FROM sessions WHERE id=?1",
            [source_session_id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .map(|value| parse_uuid("legacy C5 model invocation", value))
        .transpose()?
    };
    let Some(invocation_id) = invocation_id else {
        return Ok(CapacityC5Resolution::Unrelated);
    };
    let owned: bool = tx.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM master_no_idle_capacity_attempts a
            JOIN model_invocations m ON m.id=a.model_invocation_id
            WHERE a.model_invocation_id=?1 AND m.session_id=?2
              AND a.state IN ('capacity_failed','program_terminal')
         )",
        params![invocation_id.to_string(), source_session_id.to_string()],
        |row| row.get(0),
    )?;
    if !owned {
        return Ok(CapacityC5Resolution::Unrelated);
    }
    let changed = tx.execute("DELETE FROM daemon_settings WHERE key=?1", [pending_key])?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "capacity_c5_exact_marker_delete_failed".into(),
        ));
    }
    Ok(CapacityC5Resolution::ResolvedExact)
}

pub(super) fn c5_marker_is_capacity_owned_tx(
    tx: &Transaction<'_>,
    source_session_id: Uuid,
    pending_key: &str,
) -> Result<bool> {
    if super::daemon_settings::source_session_id_from_c5_pending_key(pending_key)
        != Some(source_session_id)
    {
        return Ok(false);
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT value FROM daemon_settings WHERE key=?1",
            [pending_key],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(false);
    };
    let Ok(marker) = super::daemon_settings::C5AutofilePending::parse(&raw) else {
        return Ok(false);
    };
    if marker.source_session_id != source_session_id {
        return Ok(false);
    }
    let invocation_id = if marker.version == 2 {
        marker.terminal_model_invocation_id
    } else {
        tx.query_row(
            "SELECT model_invocation_id FROM sessions WHERE id=?1",
            [source_session_id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .map(|value| parse_uuid("legacy C5 model invocation", value))
        .transpose()?
    };
    let Some(invocation_id) = invocation_id else {
        return Ok(false);
    };
    tx.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM master_no_idle_capacity_attempts a
            JOIN model_invocations m ON m.id=a.model_invocation_id
            WHERE a.model_invocation_id=?1 AND m.session_id=?2
              AND a.state IN ('capacity_failed','program_terminal')
         )",
        params![invocation_id.to_string(), source_session_id.to_string()],
        |row| row.get(0),
    )
    .map_err(DaemonError::Database)
}

impl Store {
    pub(crate) fn capacity_attempt_context(
        &self,
        model_invocation_id: Uuid,
    ) -> Result<Option<CapacityAttemptContext>> {
        let tx = self.conn.unchecked_transaction()?;
        load_attempt_context_tx(&tx, model_invocation_id)
    }

    pub(crate) fn resolve_c5_if_capacity_owned(
        &self,
        source_session_id: Uuid,
        pending_key: &str,
    ) -> Result<CapacityC5Ownership> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let resolution = resolve_c5_if_capacity_owned_tx(&tx, source_session_id, pending_key)?;
        tx.commit()?;
        Ok(match resolution {
            CapacityC5Resolution::ResolvedExact => CapacityC5Ownership::Owned(resolution),
            CapacityC5Resolution::AlreadyAbsent | CapacityC5Resolution::Unrelated => {
                CapacityC5Ownership::NotOwned
            }
        })
    }

    pub(crate) fn validate_unexecuted_capacity_delivery_admission(
        &self,
        request: &ModelAdmissionRequest,
        invocation_id: Uuid,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let context = capacity_admission_context_tx(&tx, request)?;
        let receipt = matching_delivery_receipt_tx(&tx, &context)?.ok_or_else(|| {
            DaemonError::Store("capacity delivery admission receipt disappeared".into())
        })?;
        if receipt.invocation_id != invocation_id
            || receipt.phase != CapacityDeliveryPhase::Admitted
        {
            return Err(DaemonError::PolicyDenied(
                "capacity delivery admission is not an exact admitted-only receipt".into(),
            ));
        }
        let job = super::scheduled_jobs::get_scheduled_job_conn(&tx, &context.wake_job_id)?
            .ok_or_else(|| DaemonError::Store("capacity delivery wake disappeared".into()))?;
        if !job.enabled || nanos(job.next_fire_at) != context.due_slot {
            return Err(DaemonError::PolicyDenied(
                "capacity delivery due slot is no longer enabled".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn is_dispatchable_unexecuted_capacity_admission(
        &self,
        invocation_id: Uuid,
    ) -> Result<bool> {
        struct ReplayEnvelope {
            attempt_state: String,
            target_raw: Option<String>,
            receipt_wake_raw: Option<String>,
            receipt_due_raw: Option<String>,
            controller_raw: String,
            incident_state: String,
            incident_due_raw: String,
            guard_raw: String,
            incident_project_raw: Option<String>,
            incident_model: Option<String>,
            incident_provider: String,
            incident_working_dir: String,
            incident_wake_raw: String,
            invocation_session_raw: Option<String>,
            invocation_project_raw: Option<String>,
            invocation_wake_raw: Option<String>,
            invocation_provider: Option<String>,
            invocation_backend: Option<String>,
            invocation_model: Option<String>,
            invocation_trigger: String,
            invocation_purpose: String,
        }

        let tx = self.conn.unchecked_transaction()?;
        let raw: Option<ReplayEnvelope> = tx
            .query_row(
                "SELECT a.state,a.resume_target_session_id,a.delivery_wake_job_id,
                        a.delivery_due_slot,i.controller_session_id,i.state,i.next_due_slot,
                        i.program_guard_job_id,i.project_id,i.model,i.provider,i.working_dir,
                        i.wake_job_id,m.session_id,m.project_id,m.scheduled_job_id,m.provider,
                        m.backend,m.model,m.trigger_source,m.purpose
                 FROM master_no_idle_capacity_attempts a
                 JOIN master_no_idle_capacity_incidents i ON i.incident_id=a.incident_id
                 JOIN model_invocations m ON m.id=a.model_invocation_id
                WHERE a.model_invocation_id=?1",
                [invocation_id.to_string()],
                |row| {
                    Ok(ReplayEnvelope {
                        attempt_state: row.get(0)?,
                        target_raw: row.get(1)?,
                        receipt_wake_raw: row.get(2)?,
                        receipt_due_raw: row.get(3)?,
                        controller_raw: row.get(4)?,
                        incident_state: row.get(5)?,
                        incident_due_raw: row.get(6)?,
                        guard_raw: row.get(7)?,
                        incident_project_raw: row.get(8)?,
                        incident_model: row.get(9)?,
                        incident_provider: row.get(10)?,
                        incident_working_dir: row.get(11)?,
                        incident_wake_raw: row.get(12)?,
                        invocation_session_raw: row.get(13)?,
                        invocation_project_raw: row.get(14)?,
                        invocation_wake_raw: row.get(15)?,
                        invocation_provider: row.get(16)?,
                        invocation_backend: row.get(17)?,
                        invocation_model: row.get(18)?,
                        invocation_trigger: row.get(19)?,
                        invocation_purpose: row.get(20)?,
                    })
                },
            )
            .optional()?;
        let Some(ReplayEnvelope {
            attempt_state,
            target_raw,
            receipt_wake_raw,
            receipt_due_raw,
            controller_raw,
            incident_state,
            incident_due_raw,
            guard_raw,
            incident_project_raw,
            incident_model,
            incident_provider,
            incident_working_dir,
            incident_wake_raw,
            invocation_session_raw,
            invocation_project_raw,
            invocation_wake_raw,
            invocation_provider,
            invocation_backend,
            invocation_model,
            invocation_trigger,
            invocation_purpose,
        }) = raw
        else {
            return Ok(false);
        };
        let (
            Some(target_raw),
            Some(receipt_wake_raw),
            Some(receipt_due_raw),
            Some(invocation_session_raw),
            Some(invocation_wake_raw),
        ) = (
            target_raw,
            receipt_wake_raw,
            receipt_due_raw,
            invocation_session_raw,
            invocation_wake_raw,
        )
        else {
            return Ok(false);
        };
        if attempt_state != "delivery_admitted"
            || incident_state != "open"
            || incident_provider != "Codex"
            || invocation_provider.as_deref() != Some("Codex")
            || invocation_backend.as_deref() != Some("Codex")
            || invocation_model != incident_model
            || invocation_trigger != "scheduled_capacity_resume"
            || invocation_purpose
                != rsi_common::model_control::ModelInvocationPurpose::SessionContinueResume.as_str()
            || target_raw != invocation_session_raw
            || receipt_wake_raw != incident_wake_raw
            || receipt_wake_raw != invocation_wake_raw
            || receipt_due_raw != incident_due_raw
            || incident_project_raw != invocation_project_raw
        {
            return Ok(false);
        }

        let parse_uuid = |value: &str| Uuid::parse_str(value).ok();
        let (Some(target), Some(controller), Some(wake), Some(guard)) = (
            parse_uuid(&target_raw),
            parse_uuid(&controller_raw),
            parse_uuid(&receipt_wake_raw),
            parse_uuid(&guard_raw),
        ) else {
            return Ok(false);
        };
        let Ok(due) =
            DateTime::parse_from_rfc3339(&receipt_due_raw).map(|value| value.with_timezone(&Utc))
        else {
            return Ok(false);
        };
        match lineage_contains_tx(&tx, target, controller) {
            Ok(true) => {}
            Ok(false) | Err(DaemonError::Store(_)) | Err(DaemonError::PolicyDenied(_)) => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        }

        let Some(wake_job) = super::scheduled_jobs::get_scheduled_job_conn(&tx, &wake)? else {
            return Ok(false);
        };
        let incident_project = incident_project_raw.as_deref().and_then(parse_uuid);
        if incident_project_raw.is_some() != incident_project.is_some()
            || !wake_job.enabled
            || wake_job.wake_mode != WakeMode::Resume
            || wake_job.wake_session_id != Some(controller)
            || !matches!(wake_job.schedule.recurrence, Recurrence::Once)
            || wake_job.schedule.anchor != due
            || wake_job.next_fire_at != due
            || wake_job.provider != Some(SessionProvider::Codex)
            || wake_job.project_id != incident_project
            || wake_job.model != incident_model
            || wake_job.working_dir.as_deref()
                != Some(PathBuf::from(&incident_working_dir).as_path())
        {
            return Ok(false);
        }

        let Some(guard_job) = super::scheduled_jobs::get_scheduled_job_conn(&tx, &guard)? else {
            return Ok(false);
        };
        Ok(guard_job.enabled
            && crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                &guard_job, controller,
            ))
    }

    pub(crate) fn unexecuted_capacity_admission_wake_id(
        &self,
        invocation_id: Uuid,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT delivery_wake_job_id
                 FROM master_no_idle_capacity_attempts
                 WHERE model_invocation_id=?1 AND state='delivery_admitted'",
                [invocation_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn confirm_capacity_delivery_launch(
        &self,
        invocation_id: Uuid,
        wake_job_id: Uuid,
        due_slot: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<CapacityLaunchConfirmation> {
        #[cfg(test)]
        if LAUNCH_CONFIRMATION_FAULT.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(DaemonError::Store(
                "capacity_launch_confirmation_transient:injected_before_transaction".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let raw: Option<(String, String, String, String, String)> = tx
            .query_row(
                "SELECT a.state,a.delivery_wake_job_id,a.delivery_due_slot,
                        i.state,i.next_due_slot
                 FROM master_no_idle_capacity_attempts a
                 JOIN master_no_idle_capacity_incidents i ON i.incident_id=a.incident_id
                 WHERE a.model_invocation_id=?1",
                [invocation_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((state, receipt_wake, receipt_due, incident_state, incident_due)) = raw else {
            return Err(DaemonError::Store(
                "capacity_launch_confirmation_receipt_missing".into(),
            ));
        };
        if parse_uuid("capacity launch wake", receipt_wake)? != wake_job_id
            || parse_nanos("capacity launch due slot", receipt_due)? != due_slot
            || incident_state != "open"
            || parse_nanos("capacity incident due slot", incident_due)? != due_slot
        {
            return Err(DaemonError::Store(
                "capacity_launch_confirmation_envelope_mismatch".into(),
            ));
        }
        let job = super::scheduled_jobs::get_scheduled_job_conn(&tx, &wake_job_id)?
            .ok_or_else(|| DaemonError::Store("capacity launch wake missing".into()))?;
        if !job.enabled || job.next_fire_at != due_slot {
            return Err(DaemonError::Store(
                "capacity_launch_confirmation_due_slot_not_enabled".into(),
            ));
        }
        match state.as_str() {
            "delivery_launch_confirmed" => {
                tx.commit()?;
                Ok(CapacityLaunchConfirmation::AlreadyConfirmed)
            }
            "delivery_admitted" => {
                let changed = tx.execute(
                    "UPDATE master_no_idle_capacity_attempts
                     SET state='delivery_launch_confirmed',delivery_launch_confirmed_at=?1,
                         updated_at=?1
                     WHERE model_invocation_id=?2 AND state='delivery_admitted'",
                    params![nanos(now), invocation_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(DaemonError::Store(
                        "capacity_launch_confirmation_transition_failed".into(),
                    ));
                }
                tx.commit()?;
                Ok(CapacityLaunchConfirmation::Committed)
            }
            _ => Err(DaemonError::Store(
                "capacity_launch_confirmation_state_conflict".into(),
            )),
        }
    }

    pub(crate) fn settle_capacity_failure(
        &self,
        source_session_id: Uuid,
        controller_session_id: Uuid,
        program_guard_job_id: Uuid,
        model_invocation_id: Uuid,
        terminal_sequence: i32,
        now: DateTime<Utc>,
    ) -> Result<CapacityFailureSettlement> {
        if terminal_sequence < 0 {
            return Err(DaemonError::Store(
                "capacity_terminal_sequence_negative".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let prior_attempt = load_attempt_context_tx(&tx, model_invocation_id)?;

        let invocation_session: Option<String> = tx
            .query_row(
                "SELECT session_id FROM model_invocations WHERE id=?1",
                [model_invocation_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let invocation_session = invocation_session.ok_or_else(|| {
            DaemonError::Store("capacity_terminal_model_invocation_missing_session".into())
        })?;
        let invocation_session = parse_uuid("terminal model Session", invocation_session)?;
        if invocation_session != source_session_id {
            return Err(DaemonError::Store(
                "capacity_terminal_source_invocation_mismatch".into(),
            ));
        }
        if !lineage_contains_tx(&tx, invocation_session, controller_session_id)? {
            return Err(DaemonError::Store(
                "capacity_terminal_invocation_outside_controller_lineage".into(),
            ));
        }

        let guard = super::scheduled_jobs::get_scheduled_job_conn(&tx, &program_guard_job_id)?;
        let guard_envelope_valid = guard.as_ref().is_some_and(|job| {
            crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                job,
                controller_session_id,
            )
        });

        let admitted_delivery = match prior_attempt {
            Some(existing) if existing.attempt_state == "capacity_failed" => {
                if existing.controller_session_id != controller_session_id
                    || existing.program_guard_job_id != program_guard_job_id
                    || !guard_envelope_valid
                {
                    return Err(DaemonError::Store(
                        "capacity_replay_incident_identity_mismatch".into(),
                    ));
                }
                let pending_key =
                    super::daemon_settings::c5_autofile_pending_key(source_session_id);
                let c5_resolution =
                    resolve_c5_if_capacity_owned_tx(&tx, source_session_id, &pending_key)?;
                let result = failure_settlement_tx(
                    &tx,
                    existing.incident_id,
                    CapacityCommitKind::Replay,
                    c5_resolution,
                )?;
                tx.commit()?;
                return Ok(result);
            }
            Some(existing) if existing.attempt_state == "delivery_launch_confirmed" => {
                Some(existing)
            }
            Some(existing) if existing.attempt_state == "delivery_admitted" => {
                return Err(DaemonError::Store(
                    "capacity_terminal_before_delivery_launch_confirmation".into(),
                ));
            }
            Some(_) => {
                return Err(DaemonError::Store(
                    "capacity_terminal_receipt_state_conflict".into(),
                ));
            }
            None => None,
        };
        if !guard_envelope_valid || guard.as_ref().is_none_or(|job| !job.enabled) {
            return Err(DaemonError::Store(
                "capacity_recovery_requires_valid_program_guard".into(),
            ));
        }

        type IncidentSnapshot = (
            String,
            i64,
            i64,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            String,
            String,
        );
        let existing: Option<IncidentSnapshot> = tx
            .query_row(
                "SELECT incident_id,outage_epoch,backoff_bucket,wake_job_id,issue_id,project_id,
                        provider,model,working_dir,opened_at
                 FROM master_no_idle_capacity_incidents
                 WHERE program_guard_job_id=?1 AND capacity_class=?2 AND state='open'",
                params![program_guard_job_id.to_string(), CAPACITY_CLASS],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ))
                },
            )
            .optional()?;

        if let Some(delivery) = admitted_delivery.as_ref() {
            if delivery.incident_state != "open"
                || delivery.controller_session_id != controller_session_id
                || delivery.program_guard_job_id != program_guard_job_id
                || existing
                    .as_ref()
                    .is_none_or(|row| row.0 != delivery.incident_id.to_string())
            {
                return Err(DaemonError::Store(
                    "capacity_delivery_terminal_incident_mismatch".into(),
                ));
            }
        }

        let (
            incident,
            epoch,
            bucket,
            wake,
            issue,
            project,
            provider,
            model,
            working_dir,
            opened_at,
        ) = if let Some((
            incident,
            epoch,
            prior_bucket,
            wake,
            issue,
            project,
            provider,
            model,
            working_dir,
            opened_at,
        )) = existing
        {
            (
                parse_uuid("incident", incident)?,
                epoch,
                (prior_bucket + 1).min(7),
                parse_uuid("capacity wake", wake)?,
                issue
                    .map(|value| parse_uuid("capacity issue", value))
                    .transpose()?,
                project
                    .map(|value| parse_uuid("capacity project", value))
                    .transpose()?,
                provider,
                model,
                working_dir,
                opened_at,
            )
        } else {
            let epoch: i64 = tx.query_row(
                "SELECT COALESCE(MAX(outage_epoch),0)+1
                 FROM master_no_idle_capacity_incidents
                 WHERE program_guard_job_id=?1 AND capacity_class=?2",
                params![program_guard_job_id.to_string(), CAPACITY_CLASS],
                |row| row.get(0),
            )?;
            let incident = incident_id(program_guard_job_id, epoch);
            let wake = wake_id(incident);
            let snapshot: Option<(
                String,
                Option<String>,
                Option<String>,
                String,
                Option<String>,
            )> = tx
                .query_row(
                    "SELECT provider,model,project_id,working_dir,sandbox_root
                     FROM sessions WHERE id=?1",
                    [controller_session_id.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((provider, model, project, working_dir, sandbox_root)) = snapshot else {
                return Err(DaemonError::SessionNotFound(controller_session_id));
            };
            if provider != "Codex" {
                return Err(DaemonError::Store(
                    "capacity_recovery_requires_codex_controller".into(),
                ));
            }
            let project = project
                .map(|value| parse_uuid("capacity project", value))
                .transpose()?;
            let issue = project.map(|_| issue_id(incident));
            (
                incident,
                epoch,
                1,
                wake,
                issue,
                project,
                provider,
                model,
                sandbox_root.unwrap_or(working_dir),
                nanos(now),
            )
        };

        if provider != "Codex" {
            return Err(DaemonError::Store(
                "capacity_incident_provider_not_codex".into(),
            ));
        }
        let due = now + Duration::seconds(BACKOFF_SECONDS[(bucket - 1) as usize]);
        let wake_row = ScheduledJob {
            id: wake,
            name: format!("provider-capacity-{incident}"),
            message: format!(
                "[rsid-capacity-recovery] Retry unattended program after Codex capacity outage {incident}."
            ),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: due,
            },
            last_fired_at: None,
            next_fire_at: due,
            enabled: true,
            working_dir: Some(PathBuf::from(&working_dir)),
            provider: Some(SessionProvider::Codex),
            model: model.clone(),
            project_id: project,
            created_at: parse_nanos("capacity opened_at", opened_at.clone())?,
            updated_at: now,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(controller_session_id),
        };
        super::scheduled_jobs::upsert_capacity_recovery_wake_tx(&tx, &wake_row)?;
        #[cfg(test)]
        settlement_fault(CapacitySettlementFault::AfterWake)?;

        if let (Some(project_id), Some(issue_id)) = (project, issue) {
            let new_issue = NewIssue {
                project_id,
                title: "Codex capacity paused unattended orchestration".into(),
                body: format!(
                    "Capacity outage epoch {epoch} for controller `{controller_session_id}`. rsid retained one same-session Resume wake `{wake}` and applies bounded backoff from 60 seconds through one hour."
                ),
                priority: Some(1),
                labels: vec![
                    "orchestration".into(),
                    "provider-capacity".into(),
                    "auto-filed".into(),
                ],
                created_by_session_id: Some(controller_session_id),
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            };
            let created = Store::idempotent_issue_in_tx(
                &tx,
                issue_id,
                &new_issue,
                super::issues::IssueWriteActor::System {
                    label: "rsi:capacity-recovery".to_string(),
                },
            )?;
            if !created.create_fields_match {
                return Err(DaemonError::Store(
                    "capacity_recovery_issue_id_conflict".into(),
                ));
            }
        }
        #[cfg(test)]
        settlement_fault(CapacitySettlementFault::AfterIssue)?;

        let existing_incident: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents WHERE incident_id=?1)",
            [incident.to_string()],
            |row| row.get(0),
        )?;
        if existing_incident {
            tx.execute(
                "UPDATE master_no_idle_capacity_incidents
                 SET backoff_bucket=?1,last_capacity_model_invocation_id=?2,
                     last_terminal_sequence=?3,next_due_slot=?4,updated_at=?5
                 WHERE incident_id=?6 AND state='open'",
                params![
                    bucket,
                    model_invocation_id.to_string(),
                    terminal_sequence,
                    nanos(due),
                    nanos(now),
                    incident.to_string()
                ],
            )?;
        } else {
            tx.execute(
                "INSERT INTO master_no_idle_capacity_incidents(
                    incident_id,program_guard_job_id,controller_session_id,capacity_class,
                    outage_epoch,state,backoff_bucket,wake_job_id,issue_id,project_id,
                    provider,model,working_dir,last_capacity_model_invocation_id,
                    last_terminal_sequence,next_due_slot,opened_at,updated_at,closed_at,close_reason
                 ) VALUES(?1,?2,?3,?4,?5,'open',?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16,NULL,NULL)",
                params![
                    incident.to_string(),
                    program_guard_job_id.to_string(),
                    controller_session_id.to_string(),
                    CAPACITY_CLASS,
                    epoch,
                    bucket,
                    wake.to_string(),
                    issue.map(|id| id.to_string()),
                    project.map(|id| id.to_string()),
                    provider,
                    model,
                    working_dir,
                    model_invocation_id.to_string(),
                    terminal_sequence,
                    nanos(due),
                    opened_at
                ],
            )?;
        }
        #[cfg(test)]
        settlement_fault(CapacitySettlementFault::AfterIncident)?;
        if admitted_delivery.is_some() {
            let changed = tx.execute(
                "UPDATE master_no_idle_capacity_attempts
                 SET state='capacity_failed',terminal_sequence=?1,
                     terminal_recorded_at=?2,updated_at=?2
                 WHERE model_invocation_id=?3 AND incident_id=?4
                   AND state='delivery_launch_confirmed'",
                params![
                    terminal_sequence,
                    nanos(now),
                    model_invocation_id.to_string(),
                    incident.to_string()
                ],
            )?;
            if changed != 1 {
                return Err(DaemonError::Store(
                    "capacity_delivery_terminal_receipt_transition_failed".into(),
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO master_no_idle_capacity_attempts(
                    incident_id,model_invocation_id,state,resume_target_session_id,
                    delivery_wake_job_id,delivery_due_slot,delivery_admitted_at,
                    delivery_launch_confirmed_at,terminal_sequence,terminal_recorded_at,
                    created_at,updated_at
                 ) VALUES(?1,?2,'capacity_failed',NULL,NULL,NULL,NULL,NULL,?3,?4,?4,?4)",
                params![
                    incident.to_string(),
                    model_invocation_id.to_string(),
                    terminal_sequence,
                    nanos(now)
                ],
            )?;
        }
        #[cfg(test)]
        settlement_fault(CapacitySettlementFault::AfterAttempt)?;
        let pending_key = super::daemon_settings::c5_autofile_pending_key(source_session_id);
        let c5_resolution = resolve_c5_if_capacity_owned_tx(&tx, source_session_id, &pending_key)?;
        #[cfg(test)]
        settlement_fault(CapacitySettlementFault::BeforeCommit)?;
        super::issues::issue_write_before_commit()?;
        tx.commit()?;
        Ok(CapacityFailureSettlement {
            commit_kind: CapacityCommitKind::New,
            incident_id: incident,
            wake_job_id: wake,
            outage_epoch: epoch,
            backoff_bucket: bucket,
            due_slot: due,
            issue_disposition: issue_disposition(issue),
            c5_resolution,
        })
    }

    pub(crate) fn settle_terminal_capacity(
        &self,
        source_session_id: Uuid,
        controller_session_id: Uuid,
        program_guard_job_id: Uuid,
        model_invocation_id: Uuid,
        terminal_sequence: i32,
        now: DateTime<Utc>,
    ) -> Result<CapacityTerminalSettlement> {
        if terminal_sequence < 0 {
            return Err(DaemonError::Store(
                "capacity_terminal_sequence_negative".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let invocation_session: Option<String> = tx
            .query_row(
                "SELECT session_id FROM model_invocations WHERE id=?1",
                [model_invocation_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let invocation_session = invocation_session.ok_or_else(|| {
            DaemonError::Store("capacity_terminal_model_invocation_missing_session".into())
        })?;
        let invocation_session = parse_uuid("terminal model Session", invocation_session)?;
        if invocation_session != source_session_id
            || !lineage_contains_tx(&tx, source_session_id, controller_session_id)?
        {
            return Err(DaemonError::Store(
                "capacity_terminal_source_or_lineage_mismatch".into(),
            ));
        }
        let guard = super::scheduled_jobs::get_scheduled_job_conn(&tx, &program_guard_job_id)?
            .ok_or_else(|| DaemonError::Store("capacity terminal program guard missing".into()))?;
        if !crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
            &guard,
            controller_session_id,
        ) {
            return Err(DaemonError::Store(
                "capacity terminal program guard malformed".into(),
            ));
        }

        let prior_attempt = load_attempt_context_tx(&tx, model_invocation_id)?;
        if let Some(existing) = prior_attempt.as_ref()
            && existing.attempt_state == "program_terminal"
        {
            if existing.controller_session_id != controller_session_id
                || existing.program_guard_job_id != program_guard_job_id
                || existing.incident_state != "closed_terminal"
                || guard.enabled
            {
                return Err(DaemonError::Store(
                    "capacity terminal replay custody mismatch".into(),
                ));
            }
            let wake_enabled: bool = tx.query_row(
                "SELECT enabled FROM scheduled_jobs WHERE id=?1",
                [existing.wake_job_id.to_string()],
                |row| row.get(0),
            )?;
            if wake_enabled {
                return Err(DaemonError::Store(
                    "capacity terminal replay wake unexpectedly enabled".into(),
                ));
            }
            let pending_key = super::daemon_settings::c5_autofile_pending_key(source_session_id);
            let c5_resolution =
                resolve_c5_if_capacity_owned_tx(&tx, source_session_id, &pending_key)?;
            let issue_raw: Option<String> = tx.query_row(
                "SELECT issue_id FROM master_no_idle_capacity_incidents WHERE incident_id=?1",
                [existing.incident_id.to_string()],
                |row| row.get(0),
            )?;
            let issue_id = issue_raw
                .map(|value| parse_uuid("capacity issue", value))
                .transpose()?;
            let result = CapacityTerminalSettlement {
                commit_kind: CapacityCommitKind::Replay,
                incident_id: existing.incident_id,
                wake_job_id: existing.wake_job_id,
                issue_disposition: issue_disposition(issue_id),
                c5_resolution,
            };
            #[cfg(test)]
            settlement_fault(CapacitySettlementFault::BeforeCommit)?;
            tx.commit()?;
            return Ok(result);
        }
        if !guard.enabled {
            return Err(DaemonError::Store(
                "capacity terminal new settlement requires enabled program guard".into(),
            ));
        }
        let delivery_context = match prior_attempt {
            Some(existing) if existing.attempt_state == "delivery_launch_confirmed" => {
                Some(existing)
            }
            Some(existing) if existing.attempt_state == "delivery_admitted" => {
                return Err(DaemonError::Store(
                    "capacity terminal before delivery launch confirmation".into(),
                ));
            }
            Some(_) => {
                return Err(DaemonError::Store(
                    "capacity terminal attempt state conflict".into(),
                ));
            }
            None => None,
        };

        type TerminalIncident = (
            String,
            i64,
            i64,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            String,
            String,
        );
        let open: Option<TerminalIncident> = tx
            .query_row(
                "SELECT incident_id,outage_epoch,backoff_bucket,wake_job_id,issue_id,project_id,
                        provider,model,working_dir,opened_at
                 FROM master_no_idle_capacity_incidents
                 WHERE program_guard_job_id=?1 AND capacity_class=?2 AND state='open'",
                params![program_guard_job_id.to_string(), CAPACITY_CLASS],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ))
                },
            )
            .optional()?;
        if let Some(delivery) = delivery_context.as_ref()
            && open
                .as_ref()
                .is_none_or(|row| row.0 != delivery.incident_id.to_string())
        {
            return Err(DaemonError::Store(
                "capacity terminal delivery incident mismatch".into(),
            ));
        }

        let (
            incident,
            epoch,
            bucket,
            wake,
            issue,
            project,
            provider,
            model,
            working_dir,
            opened_at,
        ) = if let Some((
            incident,
            epoch,
            bucket,
            wake,
            issue,
            project,
            provider,
            model,
            working_dir,
            opened_at,
        )) = open
        {
            (
                parse_uuid("incident", incident)?,
                epoch,
                bucket,
                parse_uuid("capacity wake", wake)?,
                issue
                    .map(|value| parse_uuid("capacity issue", value))
                    .transpose()?,
                project
                    .map(|value| parse_uuid("capacity project", value))
                    .transpose()?,
                provider,
                model,
                working_dir,
                opened_at,
            )
        } else {
            let epoch: i64 = tx.query_row(
                "SELECT COALESCE(MAX(outage_epoch),0)+1
                     FROM master_no_idle_capacity_incidents
                     WHERE program_guard_job_id=?1 AND capacity_class=?2",
                params![program_guard_job_id.to_string(), CAPACITY_CLASS],
                |row| row.get(0),
            )?;
            let snapshot: Option<(
                String,
                Option<String>,
                Option<String>,
                String,
                Option<String>,
            )> = tx
                .query_row(
                    "SELECT provider,model,project_id,working_dir,sandbox_root
                         FROM sessions WHERE id=?1",
                    [controller_session_id.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((provider, model, project, working_dir, sandbox_root)) = snapshot else {
                return Err(DaemonError::SessionNotFound(controller_session_id));
            };
            if provider != "Codex" {
                return Err(DaemonError::Store(
                    "capacity terminal requires Codex controller".into(),
                ));
            }
            let project = project
                .map(|value| parse_uuid("capacity project", value))
                .transpose()?;
            let incident = incident_id(program_guard_job_id, epoch);
            (
                incident,
                epoch,
                1,
                wake_id(incident),
                project.map(|_| issue_id(incident)),
                project,
                provider,
                model,
                sandbox_root.unwrap_or(working_dir),
                nanos(now),
            )
        };
        if provider != "Codex" {
            return Err(DaemonError::Store(
                "capacity terminal incident provider not Codex".into(),
            ));
        }
        let due = now + Duration::seconds(BACKOFF_SECONDS[(bucket - 1) as usize]);
        let incident_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents WHERE incident_id=?1)",
            [incident.to_string()],
            |row| row.get(0),
        )?;
        if !incident_exists {
            let wake_row = ScheduledJob {
                id: wake,
                name: format!("provider-capacity-{incident}"),
                message: format!(
                    "[rsid-capacity-recovery] Terminal capacity audit custody for unattended program {incident}."
                ),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: due,
                },
                last_fired_at: None,
                next_fire_at: due,
                enabled: false,
                working_dir: Some(PathBuf::from(&working_dir)),
                provider: Some(SessionProvider::Codex),
                model: model.clone(),
                project_id: project,
                created_at: parse_nanos("capacity opened_at", opened_at.clone())?,
                updated_at: now,
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(controller_session_id),
            };
            super::scheduled_jobs::insert_scheduled_job_conn(&tx, &wake_row)?;
            #[cfg(test)]
            settlement_fault(CapacitySettlementFault::AfterWake)?;
            if let (Some(project_id), Some(issue_id)) = (project, issue) {
                let new_issue = NewIssue {
                    project_id,
                    title: "Codex capacity paused unattended orchestration".into(),
                    body: format!(
                        "Capacity outage epoch {epoch} for controller `{controller_session_id}` terminated by a strict program outcome. Audit wake `{wake}` remains disabled."
                    ),
                    priority: Some(1),
                    labels: vec![
                        "orchestration".into(),
                        "provider-capacity".into(),
                        "auto-filed".into(),
                    ],
                    created_by_session_id: Some(controller_session_id),
                    assignee: None,
                    idea_id: None,
                    source_event_id: None,
                    source_finding_ref: None,
                };
                let created = Store::idempotent_issue_in_tx(
                    &tx,
                    issue_id,
                    &new_issue,
                    super::issues::IssueWriteActor::System {
                        label: "rsi:capacity-recovery".to_string(),
                    },
                )?;
                if !created.create_fields_match {
                    return Err(DaemonError::Store(
                        "capacity terminal issue id conflict".into(),
                    ));
                }
            }
            #[cfg(test)]
            settlement_fault(CapacitySettlementFault::AfterIssue)?;
            tx.execute(
                "INSERT INTO master_no_idle_capacity_incidents(
                    incident_id,program_guard_job_id,controller_session_id,capacity_class,
                    outage_epoch,state,backoff_bucket,wake_job_id,issue_id,project_id,
                    provider,model,working_dir,last_capacity_model_invocation_id,
                    last_terminal_sequence,next_due_slot,opened_at,updated_at,closed_at,close_reason
                 ) VALUES(?1,?2,?3,?4,?5,'closed_terminal',?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16,?16,'program_terminal')",
                params![
                    incident.to_string(), program_guard_job_id.to_string(),
                    controller_session_id.to_string(), CAPACITY_CLASS, epoch, bucket,
                    wake.to_string(), issue.map(|id| id.to_string()),
                    project.map(|id| id.to_string()), provider, model, working_dir,
                    model_invocation_id.to_string(), terminal_sequence, nanos(due), opened_at,
                ],
            )?;
        } else {
            #[cfg(test)]
            settlement_fault(CapacitySettlementFault::AfterWake)?;
            #[cfg(test)]
            settlement_fault(CapacitySettlementFault::AfterIssue)?;
            tx.execute(
                "UPDATE master_no_idle_capacity_incidents
                 SET state='closed_terminal',last_capacity_model_invocation_id=?1,
                     last_terminal_sequence=?2,closed_at=?3,close_reason='program_terminal',
                     updated_at=?3 WHERE incident_id=?4 AND state='open'",
                params![
                    model_invocation_id.to_string(),
                    terminal_sequence,
                    nanos(now),
                    incident.to_string()
                ],
            )?;
        }
        #[cfg(test)]
        settlement_fault(CapacitySettlementFault::AfterIncident)?;
        if delivery_context.is_some() {
            let changed = tx.execute(
                "UPDATE master_no_idle_capacity_attempts
                 SET state='program_terminal',terminal_sequence=?1,terminal_recorded_at=?2,
                     updated_at=?2 WHERE model_invocation_id=?3 AND incident_id=?4
                     AND state='delivery_launch_confirmed'",
                params![
                    terminal_sequence,
                    nanos(now),
                    model_invocation_id.to_string(),
                    incident.to_string()
                ],
            )?;
            if changed != 1 {
                return Err(DaemonError::Store(
                    "capacity terminal delivery transition failed".into(),
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO master_no_idle_capacity_attempts(
                    incident_id,model_invocation_id,state,resume_target_session_id,
                    delivery_wake_job_id,delivery_due_slot,delivery_admitted_at,
                    delivery_launch_confirmed_at,terminal_sequence,terminal_recorded_at,
                    created_at,updated_at
                 ) VALUES(?1,?2,'program_terminal',NULL,NULL,NULL,NULL,NULL,?3,?4,?4,?4)",
                params![
                    incident.to_string(),
                    model_invocation_id.to_string(),
                    terminal_sequence,
                    nanos(now)
                ],
            )?;
        }
        #[cfg(test)]
        settlement_fault(CapacitySettlementFault::AfterAttempt)?;
        tx.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?1 WHERE id IN (?2,?3)",
            params![
                nanos(now),
                wake.to_string(),
                program_guard_job_id.to_string()
            ],
        )?;
        let pending_key = super::daemon_settings::c5_autofile_pending_key(source_session_id);
        let c5_resolution = resolve_c5_if_capacity_owned_tx(&tx, source_session_id, &pending_key)?;
        #[cfg(test)]
        settlement_fault(CapacitySettlementFault::BeforeCommit)?;
        super::issues::issue_write_before_commit()?;
        tx.commit()?;
        Ok(CapacityTerminalSettlement {
            commit_kind: CapacityCommitKind::New,
            incident_id: incident,
            wake_job_id: wake,
            issue_disposition: issue_disposition(issue),
            c5_resolution,
        })
    }

    pub(crate) fn validate_capacity_delivery(
        &self,
        job: &ScheduledJob,
        now: DateTime<Utc>,
    ) -> Result<CapacityDeliveryValidation> {
        let tx = self.conn.unchecked_transaction()?;
        let recognized: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents WHERE wake_job_id=?1)",
            [job.id.to_string()],
            |row| row.get(0),
        )?;
        if !recognized {
            return Ok(CapacityDeliveryValidation::NotCapacity);
        }
        let raw: (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            String,
        ) = tx.query_row(
            "SELECT incident_id,controller_session_id,state,next_due_slot,project_id,model,provider,working_dir
             FROM master_no_idle_capacity_incidents WHERE wake_job_id=?1",
            [job.id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        let incident = parse_uuid("incident", raw.0)?;
        let controller = parse_uuid("controller Session", raw.1)?;
        let due = parse_nanos("capacity due slot", raw.3)?;
        let project = raw
            .4
            .map(|value| parse_uuid("capacity project", value))
            .transpose()?;
        let current = super::scheduled_jobs::get_scheduled_job_conn(&tx, &job.id)?
            .ok_or_else(|| DaemonError::Store("capacity_delivery_wake_missing".into()))?;
        let current_working_dir = current
            .working_dir
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        if raw.2 != "open"
            || current.wake_mode != WakeMode::Resume
            || current.wake_session_id != Some(controller)
            || !matches!(current.schedule.recurrence, Recurrence::Once)
            || !current.enabled
            || nanos(current.next_fire_at) != nanos(due)
            || current.project_id != project
            || current.model != raw.5
            || raw.6 != "Codex"
            || current.provider != Some(SessionProvider::Codex)
            || current_working_dir != Some(raw.7)
        {
            return Err(DaemonError::Store(
                "capacity_delivery_envelope_mismatch".into(),
            ));
        }
        let snapshot_is_current = job.wake_mode == current.wake_mode
            && job.wake_session_id == current.wake_session_id
            && job.schedule.recurrence == current.schedule.recurrence
            && job.schedule.anchor == current.schedule.anchor
            && job.next_fire_at == current.next_fire_at
            && job.enabled == current.enabled
            && job.working_dir == current.working_dir
            && job.provider == current.provider
            && job.model == current.model
            && job.project_id == current.project_id;
        if !snapshot_is_current {
            return Ok(CapacityDeliveryValidation::StaleSnapshot);
        }
        if due > now {
            return Ok(CapacityDeliveryValidation::NotDue);
        }
        Ok(CapacityDeliveryValidation::Ready(CapacityDeliveryPlan {
            incident_id: incident,
            controller_session_id: controller,
            wake_job_id: job.id,
            due_slot: due,
        }))
    }

    pub(crate) fn disable_capacity_wake(
        &self,
        wake_job_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_jobs SET enabled=0,last_fired_at=?1,updated_at=?1 WHERE id=?2",
            params![nanos(now), wake_job_id.to_string()],
        )?;
        Ok(())
    }

    pub(crate) fn disable_capacity_due_slot(
        &self,
        wake_job_id: Uuid,
        due_slot: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE scheduled_jobs SET enabled=0,last_fired_at=?1,updated_at=?1
             WHERE id=?2 AND enabled=1 AND next_fire_at=?3",
            params![nanos(now), wake_job_id.to_string(), nanos(due_slot)],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn finalize_capacity_delivery_attempt(
        &self,
        model_invocation_id: Uuid,
        terminal_sequence: i32,
        completed: bool,
        program_terminal: bool,
        now: DateTime<Utc>,
    ) -> Result<Option<CapacityAttemptContext>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(context) = load_attempt_context_tx(&tx, model_invocation_id)? else {
            return Ok(None);
        };
        if context.attempt_state == "delivery_admitted" {
            return Err(DaemonError::Store(
                "capacity delivery terminal before launch confirmation".into(),
            ));
        }
        if context.attempt_state != "delivery_launch_confirmed" {
            return Ok(Some(context));
        }
        let state = if program_terminal {
            "program_terminal"
        } else if completed {
            "non_capacity_succeeded"
        } else {
            "non_capacity_failed"
        };
        tx.execute(
            "UPDATE master_no_idle_capacity_attempts
             SET state=?1,terminal_sequence=?2,terminal_recorded_at=?3,updated_at=?3
             WHERE model_invocation_id=?4 AND state='delivery_launch_confirmed'",
            params![
                state,
                terminal_sequence,
                nanos(now),
                model_invocation_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(Some(CapacityAttemptContext {
            attempt_state: state.into(),
            ..context
        }))
    }

    pub(crate) fn close_capacity_incident(
        &self,
        source_session_id: Uuid,
        close_kind: CapacityCloseKind,
        exact_terminal_receipt: Option<(Uuid, i32)>,
        expected_program_guard: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<CapacityCloseOutcome> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let receipt_context =
            if let Some((invocation_id, _)) = exact_terminal_receipt {
                let invocation_session: Option<String> = tx
                    .query_row(
                        "SELECT session_id FROM model_invocations WHERE id=?1",
                        [invocation_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
                let invocation_session = invocation_session
                    .map(|value| parse_uuid("capacity close invocation Session", value))
                    .transpose()?;
                if invocation_session != Some(source_session_id) {
                    return Err(DaemonError::Store(
                        "capacity_close_receipt_source_mismatch".into(),
                    ));
                }
                Some(load_attempt_context_tx(&tx, invocation_id)?.ok_or_else(|| {
                    DaemonError::Store("capacity_close_exact_receipt_missing".into())
                })?)
            } else {
                None
            };
        let (selected, _lookup_stats) =
            select_open_incident_for_close_tx(&tx, source_session_id, receipt_context.as_ref())?;
        let Some((incident, wake, guard)) = selected else {
            tx.commit()?;
            return Ok(CapacityCloseOutcome::NoOpenIncident);
        };
        if let Some(expected) = expected_program_guard
            && expected != guard
        {
            return Err(DaemonError::Store(
                "capacity_close_program_guard_mismatch".into(),
            ));
        }
        let (state, reason) = match close_kind {
            CapacityCloseKind::ProgramTerminal => ("closed_terminal", "program_terminal"),
            CapacityCloseKind::NonCapacitySuccess => ("closed_success", "non_capacity_success"),
        };
        if let (Some((invocation_id, terminal_sequence)), Some(context)) =
            (exact_terminal_receipt, receipt_context)
            && context.attempt_state == "delivery_launch_confirmed"
        {
            let attempt_state = match close_kind {
                CapacityCloseKind::ProgramTerminal => "program_terminal",
                CapacityCloseKind::NonCapacitySuccess => "non_capacity_succeeded",
            };
            let changed = tx.execute(
                "UPDATE master_no_idle_capacity_attempts
                 SET state=?1,terminal_sequence=?2,terminal_recorded_at=?3,updated_at=?3
                 WHERE model_invocation_id=?4 AND incident_id=?5
                   AND state='delivery_launch_confirmed'",
                params![
                    attempt_state,
                    terminal_sequence,
                    nanos(now),
                    invocation_id.to_string(),
                    incident.to_string()
                ],
            )?;
            if changed != 1 {
                return Err(DaemonError::Store(
                    "capacity_close_terminal_receipt_transition_failed".into(),
                ));
            }
        }
        tx.execute(
            "UPDATE master_no_idle_capacity_incidents
             SET state=?1,closed_at=?2,close_reason=?3,updated_at=?2 WHERE incident_id=?4",
            params![state, nanos(now), reason, incident.to_string()],
        )?;
        tx.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?1 WHERE id=?2",
            params![nanos(now), wake.to_string()],
        )?;
        if close_kind == CapacityCloseKind::ProgramTerminal {
            tx.execute(
                "UPDATE scheduled_jobs SET enabled=0,updated_at=?1 WHERE id=?2",
                params![nanos(now), guard.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(CapacityCloseOutcome::Closed {
            incident_id: incident,
        })
    }
}

pub(crate) fn capacity_admission_context_tx(
    tx: &Transaction<'_>,
    request: &ModelAdmissionRequest,
) -> Result<CapacityAdmissionContext> {
    if request.purpose != rsi_common::model_control::ModelInvocationPurpose::SessionContinueResume
        || request.trigger != "scheduled_capacity_resume"
        || request.provider.as_deref() != Some("Codex")
        || request.backend.as_deref() != Some("Codex")
    {
        return Err(DaemonError::PolicyDenied(
            "capacity_admission_channel_envelope_mismatch".into(),
        ));
    }
    if request.owner.workflow_id.is_some()
        || request.owner.issue_tracker_id.is_some()
        || request.owner.issue_identifier.is_some()
        || request.owner.topology_node_id.is_some()
        || request.owner.recursive_graph_id.is_some()
        || request.owner.recursive_task_id.is_some()
        || request.owner.recursive_attempt_id.is_some()
        || request.owner.operator.is_some()
    {
        return Err(DaemonError::PolicyDenied(
            "capacity_admission_owner_has_extra_custody".into(),
        ));
    }
    let dedup_key = request
        .dedup_key
        .as_deref()
        .ok_or_else(|| DaemonError::PolicyDenied("capacity_admission_dedup_key_missing".into()))?;
    let rest = dedup_key
        .strip_prefix("scheduled.resume.capacity:")
        .ok_or_else(|| {
            DaemonError::PolicyDenied("capacity_admission_dedup_key_prefix_mismatch".into())
        })?;
    let (wake_raw, due_slot) = rest
        .split_once(':')
        .ok_or_else(|| DaemonError::Store("capacity_admission_dedup_key_malformed".into()))?;
    let wake = parse_uuid("capacity wake", wake_raw.to_string())?;
    if request.owner.scheduled_job_id != Some(wake) {
        return Err(DaemonError::Store(
            "capacity_admission_owner_wake_mismatch".into(),
        ));
    }
    let due = parse_nanos("capacity due slot", due_slot.to_string())?;
    if dedup_key != format!("scheduled.resume.capacity:{wake}:{}", nanos(due)) {
        return Err(DaemonError::Store(
            "capacity_admission_dedup_key_not_canonical".into(),
        ));
    }
    let Some(target) = request.owner.session_id else {
        return Err(DaemonError::Store(
            "capacity_admission_target_missing".into(),
        ));
    };
    type AdmissionIncident = (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    );
    let raw: Option<AdmissionIncident> = tx
        .query_row(
            "SELECT incident_id,controller_session_id,state,next_due_slot,project_id,model,working_dir
             FROM master_no_idle_capacity_incidents WHERE wake_job_id=?1",
            [wake.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((incident, controller, state, persisted_due, project, model, working_dir)) = raw
    else {
        return Err(DaemonError::Store(
            "capacity_admission_incident_missing".into(),
        ));
    };
    let incident = parse_uuid("incident", incident)?;
    let controller = parse_uuid("controller Session", controller)?;
    if state != "open" || nanos(due) != persisted_due {
        return Err(DaemonError::Store(
            "capacity_admission_incident_not_current".into(),
        ));
    }
    if !lineage_contains_tx(tx, target, controller)? {
        return Err(DaemonError::Store(
            "capacity_admission_target_outside_controller_lineage".into(),
        ));
    }
    let project = project
        .map(|value| parse_uuid("capacity project", value))
        .transpose()?;
    if request.owner.project_id != project
        || request.provider.as_deref() != Some("Codex")
        || request.backend.as_deref() != Some("Codex")
        || request.model != model
    {
        return Err(DaemonError::Store(
            "capacity_admission_frozen_envelope_mismatch".into(),
        ));
    }
    let job = super::scheduled_jobs::get_scheduled_job_conn(tx, &wake)?
        .ok_or_else(|| DaemonError::Store("capacity_admission_wake_missing".into()))?;
    if !job.enabled
        || job.wake_mode != WakeMode::Resume
        || !matches!(job.schedule.recurrence, Recurrence::Once)
        || job.schedule.anchor != due
        || job.next_fire_at != due
        || job.wake_session_id != Some(controller)
        || job.provider != Some(SessionProvider::Codex)
        || job.project_id != project
        || job.model != model
        || job.working_dir.as_deref() != Some(PathBuf::from(&working_dir).as_path())
    {
        return Err(DaemonError::Store(
            "capacity_admission_wake_envelope_mismatch".into(),
        ));
    }
    let session_path: Option<(
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    )> = tx
        .query_row(
            "SELECT working_dir,sandbox_root,provider,model,project_id
             FROM sessions WHERE id=?1",
            [target.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((session_working_dir, sandbox_root, session_provider, session_model, session_project)) =
        session_path
    else {
        return Err(DaemonError::SessionNotFound(target));
    };
    let session_project = session_project
        .map(|value| parse_uuid("capacity target project", value))
        .transpose()?;
    if sandbox_root.unwrap_or(session_working_dir) != working_dir
        || session_provider != "Codex"
        || session_model != model
        || session_project != project
    {
        return Err(DaemonError::Store(
            "capacity_admission_target_snapshot_mismatch".into(),
        ));
    }
    Ok(CapacityAdmissionContext {
        incident_id: incident,
        target_session_id: target,
        wake_job_id: wake,
        due_slot: nanos(due),
    })
}

pub(crate) fn matching_delivery_receipt_tx(
    tx: &Transaction<'_>,
    context: &CapacityAdmissionContext,
) -> Result<Option<CapacityDeliveryReceipt>> {
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT model_invocation_id,state FROM master_no_idle_capacity_attempts
             WHERE incident_id=?1 AND delivery_wake_job_id=?2 AND delivery_due_slot=?3
               AND resume_target_session_id=?4",
            params![
                context.incident_id.to_string(),
                context.wake_job_id.to_string(),
                context.due_slot,
                context.target_session_id.to_string()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    existing
        .map(|(value, state)| {
            let phase = match state.as_str() {
                "delivery_admitted" => CapacityDeliveryPhase::Admitted,
                "delivery_launch_confirmed"
                | "capacity_failed"
                | "non_capacity_succeeded"
                | "non_capacity_failed"
                | "program_terminal" => CapacityDeliveryPhase::LaunchConfirmed,
                _ => {
                    return Err(DaemonError::Store(format!(
                        "invalid capacity delivery receipt state: {state}"
                    )));
                }
            };
            Ok(CapacityDeliveryReceipt {
                invocation_id: parse_uuid("delivery model invocation", value)?,
                phase,
            })
        })
        .transpose()
}

pub(crate) fn insert_delivery_receipt_tx(
    tx: &Transaction<'_>,
    context: &CapacityAdmissionContext,
    invocation_id: Uuid,
    now: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO master_no_idle_capacity_attempts(
            incident_id,model_invocation_id,state,resume_target_session_id,
            delivery_wake_job_id,delivery_due_slot,delivery_admitted_at,
            delivery_launch_confirmed_at,terminal_sequence,terminal_recorded_at,
            created_at,updated_at
         ) VALUES(?1,?2,'delivery_admitted',?3,?4,?5,?6,NULL,NULL,NULL,?6,?6)",
        params![
            context.incident_id.to_string(),
            invocation_id.to_string(),
            context.target_session_id.to_string(),
            context.wake_job_id.to_string(),
            context.due_slot,
            now
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use rsi_common::types::SessionStatus;

    #[test]
    fn v89_exact_v88_full_catalog_matches_pinned_fingerprint() {
        let store = Store::open_in_memory().expect("open exact V88 derivation fixture");
        crate::store::tests::rewind_store_to_schema_version(&store.conn, 88);
        let tx = store
            .conn
            .unchecked_transaction()
            .expect("open catalog transaction");
        let fingerprint = v88_full_catalog_fingerprint(&tx).expect("fingerprint exact V88");
        assert_eq!(fingerprint, V88_ACCEPTED_FULL_CATALOG_FINGERPRINT);
    }

    #[test]
    fn v89_deployed_additive_v88_full_catalog_matches_pinned_fingerprint() {
        let directory = tempfile::tempdir().expect("create deployed-additive V88 fixture dir");
        let path = directory.path().join("deployed-additive-v88.sqlite");
        let connection = crate::store::tests::create_deployed_additive_v88_fixture(path.as_path());
        let tx = connection
            .unchecked_transaction()
            .expect("open deployed-additive catalog transaction");
        let fingerprint = v88_full_catalog_fingerprint(&tx)
            .expect("fingerprint deployed-additive exact V88 fixture");
        assert_eq!(
            fingerprint,
            V88_ACCEPTED_DEPLOYED_ADDITIVE_FULL_CATALOG_FINGERPRINT
        );
    }

    #[test]
    fn v89_pre_antigravity_v88_full_catalog_matches_pinned_fingerprint() {
        let directory = tempfile::tempdir().expect("create pre-Antigravity V88 fixture dir");
        let path = directory.path().join("pre-antigravity-v88.sqlite");
        let connection = crate::store::tests::create_v88_pre_antigravity_fixture(path.as_path());
        let tx = connection
            .unchecked_transaction()
            .expect("open pre-Antigravity catalog transaction");
        let fingerprint = v88_full_catalog_fingerprint(&tx)
            .expect("fingerprint pre-Antigravity exact V88 fixture");
        assert_eq!(
            fingerprint,
            V88_ACCEPTED_PRE_ANTIGRAVITY_FULL_CATALOG_FINGERPRINT
        );
    }

    struct Fixture {
        store: Store,
        controller: Uuid,
        guard: Uuid,
        now: DateTime<Utc>,
    }

    fn fixture(project_backed: bool) -> Fixture {
        fixture_with_store(
            Store::open_in_memory().expect("open capacity fixture"),
            project_backed,
        )
    }

    fn fixture_with_store(store: Store, project_backed: bool) -> Fixture {
        let now = Utc
            .with_ymd_and_hms(2026, 8, 22, 12, 0, 0)
            .single()
            .unwrap();
        if project_backed {
            store
                .conn
                .execute(
                    "INSERT OR IGNORE INTO projects(
                        id,name,path,description,color,context_files,created_at,updated_at
                     ) VALUES(?1,'capacity test project',NULL,NULL,'#89b4fa',NULL,?2,?2)",
                    params![super::super::d04_test_project_id().to_string(), nanos(now)],
                )
                .expect("insert capacity test project");
        }
        let mut session = crate::store::tests::make_test_session();
        session.id = Uuid::new_v4();
        session.provider = SessionProvider::Codex;
        session.model = Some("gpt-6-astra".into());
        session.status = SessionStatus::Failed;
        session.stop_reason = Some("provider_error:codex_usage_limit".into());
        session.project_id = project_backed.then(super::super::d04_test_project_id);
        let controller = session.id;
        store.insert_session(&session).expect("insert controller");
        let guard_job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
            crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                message: "program guard".into(),
                in_seconds: None,
                at: None,
                name: None,
                every_seconds: None,
                mode: Some("program_guard".into()),
                working_dir: session.working_dir.clone(),
                provider: Some(SessionProvider::Codex),
                model: session.model.clone(),
                project_id: session.project_id,
                origin_session_id: Some(controller),
                watch_session_id: None,
            },
        )
        .expect("build exact program guard");
        let guard = guard_job.id;
        store
            .insert_scheduled_job(&guard_job)
            .expect("insert program guard");
        Fixture {
            store,
            controller,
            guard,
            now,
        }
    }

    fn insert_invocation(fixture: &Fixture, invocation: Uuid) {
        fixture
            .store
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    trigger_source,session_id,policy_snapshot_json,usage_confidence,
                    created_at,completed_at
                 ) VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                          'admitted','failed','capacity-test',?2,'{}','unavailable',?3,?3)",
                params![
                    invocation.to_string(),
                    fixture.controller.to_string(),
                    nanos(fixture.now)
                ],
            )
            .expect("insert model invocation");
    }

    fn count(store: &Store, table: &str) -> i64 {
        store
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count capacity rows")
    }

    fn capacity_custody_snapshot(store: &Store) -> (String, String, String, String, String) {
        store
            .conn
            .query_row(
                "SELECT
                    COALESCE((SELECT group_concat(value,'|') FROM (
                        SELECT id || ':' || enabled || ':' || next_fire_at AS value
                        FROM scheduled_jobs ORDER BY id)),''),
                    COALESCE((SELECT group_concat(value,'|') FROM (
                        SELECT incident_id || ':' || controller_session_id || ':' || state || ':' ||
                               wake_job_id || ':' || coalesce(closed_at,'') || ':' || coalesce(close_reason,'') AS value
                        FROM master_no_idle_capacity_incidents ORDER BY incident_id)),''),
                    COALESCE((SELECT group_concat(value,'|') FROM (
                        SELECT model_invocation_id || ':' || incident_id || ':' || state || ':' ||
                               coalesce(terminal_sequence,'') AS value
                        FROM master_no_idle_capacity_attempts ORDER BY model_invocation_id)),''),
                    COALESCE((SELECT group_concat(value,'|') FROM (
                        SELECT id || ':' || status AS value FROM issues ORDER BY id)),''),
                    COALESCE((SELECT group_concat(value,'|') FROM (
                        SELECT key || ':' || value AS value FROM daemon_settings
                        WHERE key LIKE 'c5_autofile_pending:%' ORDER BY key)), '')",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("snapshot capacity custody")
    }

    fn add_open_outage(
        store: &Store,
        continued_from: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> (Uuid, Uuid, Uuid, CapacityFailureSettlement) {
        let mut session = crate::store::tests::make_test_session();
        session.id = Uuid::new_v4();
        session.provider = SessionProvider::Codex;
        session.model = Some("gpt-6-astra".into());
        session.status = SessionStatus::Failed;
        session.stop_reason = Some("provider_error:codex_usage_limit".into());
        session.project_id = None;
        session.continued_from = continued_from;
        let controller = session.id;
        store
            .insert_session(&session)
            .expect("insert outage controller");
        let guard_job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
            crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                message: "program guard".into(),
                in_seconds: None,
                at: None,
                name: None,
                every_seconds: None,
                mode: Some("program_guard".into()),
                working_dir: session.working_dir.clone(),
                provider: Some(SessionProvider::Codex),
                model: session.model.clone(),
                project_id: None,
                origin_session_id: Some(controller),
                watch_session_id: None,
            },
        )
        .expect("build outage guard");
        store
            .insert_scheduled_job(&guard_job)
            .expect("insert outage guard");
        let invocation = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    trigger_source,session_id,policy_snapshot_json,usage_confidence,
                    created_at,completed_at
                 ) VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                          'admitted','failed','capacity-close-test',?2,'{}','unavailable',?3,?3)",
                params![invocation.to_string(), controller.to_string(), nanos(now)],
            )
            .expect("insert outage invocation");
        let settlement = store
            .settle_capacity_failure(controller, controller, guard_job.id, invocation, 1, now)
            .expect("open outage");
        (controller, guard_job.id, invocation, settlement)
    }

    fn attributed_issue(settlement: &CapacityFailureSettlement) -> Option<Uuid> {
        match settlement.issue_disposition {
            CapacityIssueDisposition::Attributed { issue_id } => Some(issue_id),
            CapacityIssueDisposition::Projectless => None,
        }
    }

    #[test]
    fn capacity_backoff_table_is_exact_and_saturating() {
        assert_eq!(BACKOFF_SECONDS, [60, 120, 240, 480, 960, 1920, 3600]);
        assert_eq!(BACKOFF_SECONDS[6], 3600);
    }

    #[test]
    fn capacity_identities_are_stable_and_epoch_scoped() {
        let guard = Uuid::from_u128(1);
        let first = incident_id(guard, 1);
        assert_eq!(first, incident_id(guard, 1));
        assert_ne!(first, incident_id(guard, 2));
        assert_ne!(wake_id(first), issue_id(first));
    }

    #[test]
    fn initial_replay_and_eight_distinct_attempts_are_one_bounded_outage() {
        let fixture = fixture(true);
        let mut stable = None;
        let expected = [60, 120, 240, 480, 960, 1920, 3600, 3600];
        for (index, delay) in expected.into_iter().enumerate() {
            let invocation = Uuid::new_v4();
            insert_invocation(&fixture, invocation);
            let at = fixture.now + Duration::hours(index as i64);
            let result = fixture
                .store
                .settle_capacity_failure(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    index as i32,
                    at,
                )
                .expect("settle new capacity attempt");
            assert_eq!(result.backoff_bucket, (index as i64 + 1).min(7));
            assert_eq!(result.due_slot, at + Duration::seconds(delay));
            let identities = (
                result.incident_id,
                result.wake_job_id,
                attributed_issue(&result),
            );
            assert_eq!(*stable.get_or_insert(identities), identities);

            let replay = fixture
                .store
                .settle_capacity_failure(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    index as i32,
                    at + Duration::seconds(1),
                )
                .expect("replay capacity receipt");
            assert_eq!(replay.commit_kind, CapacityCommitKind::Replay);
            assert_eq!(replay.backoff_bucket, result.backoff_bucket);
            assert_eq!(replay.due_slot, result.due_slot);
        }
        assert_eq!(
            count(&fixture.store, "master_no_idle_capacity_incidents"),
            1
        );
        assert_eq!(count(&fixture.store, "master_no_idle_capacity_attempts"), 8);
        assert_eq!(count(&fixture.store, "issues"), 1);
        assert_eq!(
            fixture
                .store
                .conn
                .query_row(
                    "SELECT count(*) FROM scheduled_jobs j
                     JOIN master_no_idle_capacity_incidents i ON i.wake_job_id=j.id
                     WHERE j.enabled=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn projectless_outage_stays_wake_only_until_a_new_epoch() {
        let fixture = fixture(false);
        let first_invocation = Uuid::new_v4();
        insert_invocation(&fixture, first_invocation);
        let first = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                first_invocation,
                1,
                fixture.now,
            )
            .unwrap();
        assert_eq!(attributed_issue(&first), None);
        assert_eq!(count(&fixture.store, "issues"), 0);

        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?1 WHERE id=?2",
                params![
                    super::super::d04_test_project_id().to_string(),
                    fixture.controller.to_string()
                ],
            )
            .unwrap();
        let second_invocation = Uuid::new_v4();
        insert_invocation(&fixture, second_invocation);
        let second = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                second_invocation,
                2,
                fixture.now + Duration::minutes(2),
            )
            .unwrap();
        assert_eq!(second.incident_id, first.incident_id);
        assert_eq!(attributed_issue(&second), None);
        assert_eq!(count(&fixture.store, "issues"), 0);

        assert!(matches!(
            fixture
                .store
                .close_capacity_incident(
                    fixture.controller,
                    CapacityCloseKind::NonCapacitySuccess,
                    None,
                    None,
                    fixture.now + Duration::minutes(3),
                )
                .unwrap(),
            CapacityCloseOutcome::Closed { .. }
        ));
        let third_invocation = Uuid::new_v4();
        insert_invocation(&fixture, third_invocation);
        let third = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                third_invocation,
                3,
                fixture.now + Duration::minutes(4),
            )
            .unwrap();
        assert_eq!(third.outage_epoch, 2);
        assert_ne!(third.incident_id, first.incident_id);
        assert!(attributed_issue(&third).is_some());
        assert_eq!(count(&fixture.store, "issues"), 1);
    }

    #[test]
    fn issue_writer_settlement_failpoints_roll_back_wake_issue_incident_event_and_attempt() {
        for fault in [
            CapacitySettlementFault::AfterWake,
            CapacitySettlementFault::AfterIssue,
            CapacitySettlementFault::AfterIncident,
            CapacitySettlementFault::AfterAttempt,
            CapacitySettlementFault::BeforeCommit,
        ] {
            let fixture = fixture(true);
            let invocation = Uuid::new_v4();
            insert_invocation(&fixture, invocation);
            test_fail_next_settlement(fault);
            let error = fixture
                .store
                .settle_capacity_failure(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    9,
                    fixture.now,
                )
                .expect_err("injected settlement must abort");
            assert!(error.to_string().contains("capacity_recovery_transient"));
            assert_eq!(
                count(&fixture.store, "master_no_idle_capacity_incidents"),
                0
            );
            assert_eq!(count(&fixture.store, "master_no_idle_capacity_attempts"), 0);
            assert_eq!(count(&fixture.store, "issues"), 0);
            assert_eq!(count(&fixture.store, "issue_events"), 0);
            assert_eq!(count(&fixture.store, "scheduled_jobs"), 1);

            let committed = fixture
                .store
                .settle_capacity_failure(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    9,
                    fixture.now,
                )
                .expect("retry commits one complete outage");
            assert_eq!(committed.commit_kind, CapacityCommitKind::New);
            let issue_id = attributed_issue(&committed).expect("project capacity Issue");
            let history = fixture
                .store
                .list_issue_events_v1(&rsi_common::types::IssueEventPageRequestV1 {
                    issue_id,
                    after_sequence: 0,
                    limit: None,
                })
                .unwrap();
            assert_eq!(history.events.len(), 1);
            let event = &history.events[0];
            assert_eq!(
                event.actor_kind,
                rsi_common::types::IssueActorKindV1::System
            );
            assert_eq!(event.actor_label.as_deref(), Some("rsi:capacity-recovery"));
            assert_eq!(
                event.request.fingerprint().unwrap(),
                event.request_fingerprint
            );
            assert!(matches!(
                &event.request.operation,
                rsi_common::types::IssueSemanticOperationV1::Created { create }
                    if create.issue_id == issue_id
                        && create.project_id == event.project_id
                        && create.created_by_session_id == Some(fixture.controller)
                        && create.title == event.issue.title
                        && create.body == event.issue.body
                        && create.labels == event.issue.labels
            ));
            assert_eq!(
                fixture
                    .store
                    .settle_capacity_failure(
                        fixture.controller,
                        fixture.controller,
                        fixture.guard,
                        invocation,
                        9,
                        fixture.now + Duration::seconds(1),
                    )
                    .unwrap()
                    .commit_kind,
                CapacityCommitKind::Replay
            );
            assert_eq!(count(&fixture.store, "issue_events"), 1);
        }
    }

    #[test]
    fn close_is_forward_only_and_replay_cannot_reopen_or_reschedule() {
        let fixture = fixture(true);
        let invocation = Uuid::new_v4();
        insert_invocation(&fixture, invocation);
        let result = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                invocation,
                4,
                fixture.now,
            )
            .unwrap();
        assert!(matches!(
            fixture
                .store
                .close_capacity_incident(
                    fixture.controller,
                    CapacityCloseKind::ProgramTerminal,
                    None,
                    Some(fixture.guard),
                    fixture.now + Duration::minutes(2),
                )
                .unwrap(),
            CapacityCloseOutcome::Closed { .. }
        ));
        let replay = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                invocation,
                4,
                fixture.now + Duration::hours(1),
            )
            .unwrap();
        assert_eq!(replay.commit_kind, CapacityCommitKind::Replay);
        assert_eq!(replay.incident_id, result.incident_id);
        assert_eq!(replay.due_slot, result.due_slot);
        assert_eq!(
            fixture
                .store
                .conn
                .query_row(
                    "SELECT state FROM master_no_idle_capacity_incidents WHERE incident_id=?1",
                    [result.incident_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "closed_terminal"
        );
        assert!(
            !fixture
                .store
                .get_scheduled_job(&result.wake_job_id)
                .unwrap()
                .unwrap()
                .enabled
        );
        assert!(
            !fixture
                .store
                .get_scheduled_job(&fixture.guard)
                .unwrap()
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn fast_capacity_terminal_rearm_wins_over_stale_scheduler_disable() {
        let fixture = fixture(true);
        let opening_invocation = Uuid::new_v4();
        insert_invocation(&fixture, opening_invocation);
        let opening = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                opening_invocation,
                1,
                fixture.now,
            )
            .unwrap();

        let delivery_invocation = Uuid::new_v4();
        insert_invocation(&fixture, delivery_invocation);
        fixture
            .store
            .conn
            .execute(
                "INSERT INTO master_no_idle_capacity_attempts(
                    incident_id,model_invocation_id,state,resume_target_session_id,
                    delivery_wake_job_id,delivery_due_slot,delivery_admitted_at,
                    terminal_sequence,terminal_recorded_at,created_at,updated_at
                 ) VALUES(?1,?2,'delivery_admitted',?3,?4,?5,?6,NULL,NULL,?6,?6)",
                params![
                    opening.incident_id.to_string(),
                    delivery_invocation.to_string(),
                    fixture.controller.to_string(),
                    opening.wake_job_id.to_string(),
                    nanos(opening.due_slot),
                    nanos(fixture.now + Duration::seconds(61))
                ],
            )
            .unwrap();
        fixture
            .store
            .confirm_capacity_delivery_launch(
                delivery_invocation,
                opening.wake_job_id,
                opening.due_slot,
                fixture.now + Duration::seconds(61),
            )
            .unwrap();
        let rearmed_at = fixture.now + Duration::seconds(62);
        let advanced = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                delivery_invocation,
                2,
                rearmed_at,
            )
            .unwrap();
        assert_eq!(advanced.backoff_bucket, 2);
        assert_eq!(advanced.due_slot, rearmed_at + Duration::seconds(120));
        assert!(
            !fixture
                .store
                .disable_capacity_due_slot(
                    opening.wake_job_id,
                    opening.due_slot,
                    rearmed_at + Duration::seconds(1),
                )
                .unwrap()
        );
        let wake = fixture
            .store
            .get_scheduled_job(&opening.wake_job_id)
            .unwrap()
            .unwrap();
        assert!(wake.enabled);
        assert_eq!(wake.next_fire_at, advanced.due_slot);
        let replay = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                delivery_invocation,
                2,
                rearmed_at + Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(replay.commit_kind, CapacityCommitKind::Replay);
        assert_eq!(replay.backoff_bucket, 2);
        assert_eq!(replay.due_slot, advanced.due_slot);
    }

    #[test]
    fn terminal_replay_after_store_reopen_is_a_noop() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capacity-reopen.sqlite");
        let fixture = fixture_with_store(Store::open(&database).unwrap(), true);
        let invocation = Uuid::new_v4();
        insert_invocation(&fixture, invocation);
        let committed = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                invocation,
                8,
                fixture.now,
            )
            .unwrap();
        let issue_id = attributed_issue(&committed).expect("project-backed capacity issue");
        let controller = fixture.controller;
        let guard = fixture.guard;
        let now = fixture.now;
        drop(fixture);

        let reopened = Store::open(&database).unwrap();
        let replay = reopened
            .settle_capacity_failure(
                controller,
                controller,
                guard,
                invocation,
                99,
                now + Duration::hours(4),
            )
            .unwrap();
        assert_eq!(replay.commit_kind, CapacityCommitKind::Replay);
        assert_eq!(replay.incident_id, committed.incident_id);
        assert_eq!(replay.wake_job_id, committed.wake_job_id);
        assert_eq!(attributed_issue(&replay), attributed_issue(&committed));
        assert_eq!(replay.backoff_bucket, committed.backoff_bucket);
        assert_eq!(replay.due_slot, committed.due_slot);
        assert_eq!(count(&reopened, "master_no_idle_capacity_incidents"), 1);
        assert_eq!(count(&reopened, "master_no_idle_capacity_attempts"), 1);
        assert_eq!(count(&reopened, "issues"), 1);
        assert_eq!(attributed_issue(&replay), Some(issue_id));
        assert_eq!(
            reopened
                .conn
                .query_row(
                    "SELECT count(*) FROM issues WHERE id=?1 AND project_id=?2",
                    params![
                        issue_id.to_string(),
                        super::super::d04_test_project_id().to_string()
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            reopened
                .conn
                .query_row(
                    "SELECT count(*) FROM scheduled_jobs j
                     JOIN master_no_idle_capacity_incidents i ON i.wake_job_id=j.id
                     WHERE j.enabled=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let wake = reopened
            .get_scheduled_job(&committed.wake_job_id)
            .unwrap()
            .expect("stable capacity wake after reopen");
        assert!(wake.enabled);
        assert_eq!(wake.id, committed.wake_job_id);
        assert_eq!(wake.next_fire_at, committed.due_slot);
    }

    #[test]
    fn noncapacity_success_atomically_closes_delivery_receipt_and_resets_epoch() {
        let fixture = fixture(true);
        let opening_invocation = Uuid::new_v4();
        insert_invocation(&fixture, opening_invocation);
        let opening = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                opening_invocation,
                1,
                fixture.now,
            )
            .unwrap();
        let success_invocation = Uuid::new_v4();
        insert_invocation(&fixture, success_invocation);
        fixture
            .store
            .conn
            .execute(
                "INSERT INTO master_no_idle_capacity_attempts(
                    incident_id,model_invocation_id,state,resume_target_session_id,
                    delivery_wake_job_id,delivery_due_slot,delivery_admitted_at,
                    terminal_sequence,terminal_recorded_at,created_at,updated_at
                 ) VALUES(?1,?2,'delivery_admitted',?3,?4,?5,?6,NULL,NULL,?6,?6)",
                params![
                    opening.incident_id.to_string(),
                    success_invocation.to_string(),
                    fixture.controller.to_string(),
                    opening.wake_job_id.to_string(),
                    nanos(opening.due_slot),
                    nanos(fixture.now + Duration::seconds(61))
                ],
            )
            .unwrap();
        fixture
            .store
            .confirm_capacity_delivery_launch(
                success_invocation,
                opening.wake_job_id,
                opening.due_slot,
                fixture.now + Duration::seconds(61),
            )
            .unwrap();
        assert!(matches!(
            fixture
                .store
                .close_capacity_incident(
                    fixture.controller,
                    CapacityCloseKind::NonCapacitySuccess,
                    Some((success_invocation, 2)),
                    None,
                    fixture.now + Duration::seconds(62),
                )
                .unwrap(),
            CapacityCloseOutcome::Closed { .. }
        ));
        assert_eq!(
            fixture
                .store
                .conn
                .query_row(
                    "SELECT i.state,a.state FROM master_no_idle_capacity_incidents i
                     JOIN master_no_idle_capacity_attempts a ON a.incident_id=i.incident_id
                     WHERE a.model_invocation_id=?1",
                    [success_invocation.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            ("closed_success".into(), "non_capacity_succeeded".into())
        );
        assert!(
            fixture
                .store
                .get_scheduled_job(&fixture.guard)
                .unwrap()
                .unwrap()
                .enabled,
            "success closes only the capacity wake; ordinary no-idle still evaluates the sentinel"
        );

        let next_invocation = Uuid::new_v4();
        insert_invocation(&fixture, next_invocation);
        let next = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                next_invocation,
                3,
                fixture.now + Duration::minutes(5),
            )
            .unwrap();
        assert_eq!(next.outage_epoch, 2);
        assert_eq!(next.backoff_bucket, 1);
        assert_ne!(next.incident_id, opening.incident_id);
        assert_ne!(attributed_issue(&next), attributed_issue(&opening));
    }

    #[test]
    fn issue_writer_terminal_capacity_new_replay_and_project_issue_disposition_are_exact() {
        for project_backed in [true, false] {
            let fixture = fixture(project_backed);
            let invocation = Uuid::new_v4();
            insert_invocation(&fixture, invocation);
            fixture
                .store
                .set_session_model_invocation(fixture.controller, Some(invocation))
                .unwrap();
            fixture
                .store
                .update_failed_and_stage_c5_autofile(
                    fixture.controller,
                    super::super::daemon_settings::AutofileCause::ProcessDied,
                )
                .unwrap();
            let first = fixture
                .store
                .settle_terminal_capacity(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    11,
                    fixture.now,
                )
                .unwrap();
            assert_eq!(first.commit_kind, CapacityCommitKind::New);
            assert_eq!(first.c5_resolution, CapacityC5Resolution::ResolvedExact);
            assert_eq!(
                matches!(
                    first.issue_disposition,
                    CapacityIssueDisposition::Attributed { .. }
                ),
                project_backed
            );
            let replay = fixture
                .store
                .settle_terminal_capacity(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    999,
                    fixture.now + Duration::minutes(1),
                )
                .unwrap();
            assert_eq!(replay.commit_kind, CapacityCommitKind::Replay);
            assert_eq!(replay.incident_id, first.incident_id);
            assert_eq!(replay.wake_job_id, first.wake_job_id);
            assert_eq!(count(&fixture.store, "issues"), i64::from(project_backed));
            assert_eq!(
                count(&fixture.store, "issue_events"),
                i64::from(project_backed)
            );
            if let CapacityIssueDisposition::Attributed { issue_id } = first.issue_disposition {
                let history = fixture
                    .store
                    .list_issue_events_v1(&rsi_common::types::IssueEventPageRequestV1 {
                        issue_id,
                        after_sequence: 0,
                        limit: None,
                    })
                    .unwrap();
                assert_eq!(history.events.len(), 1);
                let event = &history.events[0];
                assert_eq!(
                    event.actor_kind,
                    rsi_common::types::IssueActorKindV1::System
                );
                assert_eq!(event.actor_label.as_deref(), Some("rsi:capacity-recovery"));
                assert_eq!(
                    event.request.fingerprint().unwrap(),
                    event.request_fingerprint
                );
            }
            assert_eq!(
                fixture
                    .store
                    .conn
                    .query_row(
                        "SELECT state FROM master_no_idle_capacity_incidents WHERE incident_id=?1",
                        [first.incident_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
                "closed_terminal"
            );
            assert_eq!(
                fixture
                    .store
                    .conn
                    .query_row(
                        "SELECT state FROM master_no_idle_capacity_attempts WHERE model_invocation_id=?1",
                        [invocation.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
                "program_terminal"
            );
            assert!(
                !fixture
                    .store
                    .get_scheduled_job(&fixture.guard)
                    .unwrap()
                    .unwrap()
                    .enabled
            );
            assert!(
                !fixture
                    .store
                    .get_scheduled_job(&first.wake_job_id)
                    .unwrap()
                    .unwrap()
                    .enabled
            );
        }
    }

    #[test]
    fn terminal_capacity_fault_seams_roll_back_every_custody_edge() {
        for fault in [
            CapacitySettlementFault::AfterWake,
            CapacitySettlementFault::AfterIssue,
            CapacitySettlementFault::AfterIncident,
            CapacitySettlementFault::AfterAttempt,
            CapacitySettlementFault::BeforeCommit,
        ] {
            let fixture = fixture(true);
            let invocation = Uuid::new_v4();
            insert_invocation(&fixture, invocation);
            fixture
                .store
                .set_session_model_invocation(fixture.controller, Some(invocation))
                .unwrap();
            fixture
                .store
                .update_failed_and_stage_c5_autofile(
                    fixture.controller,
                    super::super::daemon_settings::AutofileCause::ProcessDied,
                )
                .unwrap();
            let before = capacity_custody_snapshot(&fixture.store);
            test_fail_next_settlement(fault);
            let error = fixture
                .store
                .settle_terminal_capacity(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    12,
                    fixture.now,
                )
                .expect_err("terminal settlement fault must abort");
            assert!(error.to_string().contains("capacity_recovery_transient"));
            assert_eq!(capacity_custody_snapshot(&fixture.store), before);
            let committed = fixture
                .store
                .settle_terminal_capacity(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    12,
                    fixture.now,
                )
                .expect("retry commits complete terminal custody");
            assert_eq!(committed.commit_kind, CapacityCommitKind::New);
        }
    }

    #[test]
    fn terminal_capacity_reopen_replays_without_rearm_for_both_issue_dispositions() {
        for project_backed in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let database = directory
                .path()
                .join(format!("terminal-{project_backed}.sqlite"));
            let fixture = fixture_with_store(Store::open(&database).unwrap(), project_backed);
            let invocation = Uuid::new_v4();
            insert_invocation(&fixture, invocation);
            let first = fixture
                .store
                .settle_terminal_capacity(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    invocation,
                    13,
                    fixture.now,
                )
                .unwrap();
            let controller = fixture.controller;
            let guard = fixture.guard;
            let now = fixture.now;
            drop(fixture);

            let reopened = Store::open(&database).unwrap();
            let replay = reopened
                .settle_terminal_capacity(
                    controller,
                    controller,
                    guard,
                    invocation,
                    13,
                    now + Duration::hours(1),
                )
                .unwrap();
            assert_eq!(replay.commit_kind, CapacityCommitKind::Replay);
            assert_eq!(replay.incident_id, first.incident_id);
            assert_eq!(count(&reopened, "issues"), i64::from(project_backed));
            assert!(!reopened.get_scheduled_job(&guard).unwrap().unwrap().enabled);
            assert!(
                !reopened
                    .get_scheduled_job(&first.wake_job_id)
                    .unwrap()
                    .unwrap()
                    .enabled
            );
        }
    }

    #[test]
    fn terminal_capacity_delivery_receipt_transitions_once_and_replays_exactly() {
        let fixture = fixture(true);
        let opening_invocation = Uuid::new_v4();
        insert_invocation(&fixture, opening_invocation);
        let opening = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                opening_invocation,
                1,
                fixture.now,
            )
            .unwrap();
        let delivery_invocation = Uuid::new_v4();
        insert_invocation(&fixture, delivery_invocation);
        fixture
            .store
            .conn
            .execute(
                "INSERT INTO master_no_idle_capacity_attempts(
                    incident_id,model_invocation_id,state,resume_target_session_id,
                    delivery_wake_job_id,delivery_due_slot,delivery_admitted_at,
                    terminal_sequence,terminal_recorded_at,created_at,updated_at
                 ) VALUES(?1,?2,'delivery_admitted',?3,?4,?5,?6,NULL,NULL,?6,?6)",
                params![
                    opening.incident_id.to_string(),
                    delivery_invocation.to_string(),
                    fixture.controller.to_string(),
                    opening.wake_job_id.to_string(),
                    nanos(opening.due_slot),
                    nanos(fixture.now + Duration::seconds(61))
                ],
            )
            .unwrap();
        fixture
            .store
            .confirm_capacity_delivery_launch(
                delivery_invocation,
                opening.wake_job_id,
                opening.due_slot,
                fixture.now + Duration::seconds(62),
            )
            .unwrap();
        fixture
            .store
            .set_session_model_invocation(fixture.controller, Some(delivery_invocation))
            .unwrap();
        fixture
            .store
            .update_failed_and_stage_c5_autofile(
                fixture.controller,
                super::super::daemon_settings::AutofileCause::ProcessDied,
            )
            .unwrap();
        let first = fixture
            .store
            .settle_terminal_capacity(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                delivery_invocation,
                2,
                fixture.now + Duration::seconds(63),
            )
            .unwrap();
        assert_eq!(first.commit_kind, CapacityCommitKind::New);
        assert_eq!(first.incident_id, opening.incident_id);
        assert_eq!(first.c5_resolution, CapacityC5Resolution::ResolvedExact);
        assert_eq!(
            fixture
                .store
                .conn
                .query_row(
                    "SELECT state FROM master_no_idle_capacity_attempts WHERE model_invocation_id=?1",
                    [delivery_invocation.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "program_terminal"
        );
        assert_eq!(
            fixture
                .store
                .settle_terminal_capacity(
                    fixture.controller,
                    fixture.controller,
                    fixture.guard,
                    delivery_invocation,
                    999,
                    fixture.now + Duration::minutes(2),
                )
                .unwrap()
                .commit_kind,
            CapacityCommitKind::Replay
        );
    }

    #[test]
    fn close_lookup_is_lineage_bounded_and_ignores_101_unrelated_incidents() {
        let fixture = fixture(false);
        let invocation = Uuid::new_v4();
        insert_invocation(&fixture, invocation);
        let target = fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                invocation,
                1,
                fixture.now,
            )
            .unwrap();
        let (selected_before, stats_before) = {
            let tx = fixture.store.conn.unchecked_transaction().unwrap();
            select_open_incident_for_close_tx(&tx, fixture.controller, None).unwrap()
        };
        assert_eq!(selected_before.unwrap().0, target.incident_id);
        assert_eq!(stats_before.ancestry_nodes, 1);
        assert_eq!(stats_before.controller_index_lookups, 1);

        for offset in 1..=101 {
            add_open_outage(
                &fixture.store,
                None,
                fixture.now + Duration::seconds(offset),
            );
        }
        let (selected_after, stats_after) = {
            let tx = fixture.store.conn.unchecked_transaction().unwrap();
            select_open_incident_for_close_tx(&tx, fixture.controller, None).unwrap()
        };
        assert_eq!(selected_after, selected_before);
        assert_eq!(stats_after, stats_before);

        let (selected_exact, exact_stats) = {
            let tx = fixture.store.conn.unchecked_transaction().unwrap();
            let receipt = load_attempt_context_tx(&tx, invocation).unwrap().unwrap();
            select_open_incident_for_close_tx(&tx, fixture.controller, Some(&receipt)).unwrap()
        };
        assert_eq!(selected_exact, selected_before);
        assert_eq!(exact_stats.ancestry_nodes, 1);
        assert_eq!(exact_stats.controller_index_lookups, 0);
    }

    #[test]
    fn receipt_free_close_uses_newest_opened_then_incident_id_tie_break() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc
            .with_ymd_and_hms(2026, 8, 22, 18, 0, 0)
            .single()
            .unwrap();
        let (parent, _, _, parent_settlement) = add_open_outage(&store, None, now);
        let (child, _, _, child_settlement) = add_open_outage(&store, Some(parent), now);
        let (selected, stats) = {
            let tx = store.conn.unchecked_transaction().unwrap();
            select_open_incident_for_close_tx(&tx, child, None).unwrap()
        };
        let expected = [parent_settlement.incident_id, child_settlement.incident_id]
            .into_iter()
            .max_by_key(Uuid::to_string)
            .unwrap();
        assert_eq!(selected.unwrap().0, expected);
        assert_eq!(stats.ancestry_nodes, 2);
        assert_eq!(stats.controller_index_lookups, 2);
    }

    #[test]
    fn close_corruption_matrix_fails_without_custody_mutation() {
        let fixture = fixture(false);
        let invocation = Uuid::new_v4();
        insert_invocation(&fixture, invocation);
        fixture
            .store
            .settle_capacity_failure(
                fixture.controller,
                fixture.controller,
                fixture.guard,
                invocation,
                1,
                fixture.now,
            )
            .unwrap();

        let before_missing = capacity_custody_snapshot(&fixture.store);
        let error = fixture
            .store
            .close_capacity_incident(
                Uuid::new_v4(),
                CapacityCloseKind::NonCapacitySuccess,
                None,
                None,
                fixture.now,
            )
            .expect_err("missing ancestry must fail closed");
        assert!(
            error
                .to_string()
                .contains("capacity_close_lineage_missing_session")
        );
        assert_eq!(capacity_custody_snapshot(&fixture.store), before_missing);

        let mut cyclic = crate::store::tests::make_test_session();
        cyclic.id = Uuid::new_v4();
        cyclic.continued_from = Some(cyclic.id);
        let cyclic_id = cyclic.id;
        fixture.store.insert_session(&cyclic).unwrap();
        let before_cycle = capacity_custody_snapshot(&fixture.store);
        let error = fixture
            .store
            .close_capacity_incident(
                cyclic_id,
                CapacityCloseKind::NonCapacitySuccess,
                None,
                None,
                fixture.now,
            )
            .expect_err("cyclic ancestry must fail closed");
        assert!(error.to_string().contains("capacity_close_lineage_cycle"));
        assert_eq!(capacity_custody_snapshot(&fixture.store), before_cycle);

        let before_receipt = capacity_custody_snapshot(&fixture.store);
        let error = fixture
            .store
            .close_capacity_incident(
                Uuid::new_v4(),
                CapacityCloseKind::ProgramTerminal,
                Some((invocation, 1)),
                None,
                fixture.now,
            )
            .expect_err("receipt source mismatch must fail closed");
        assert!(
            error
                .to_string()
                .contains("capacity_close_receipt_source_mismatch")
        );
        assert_eq!(capacity_custody_snapshot(&fixture.store), before_receipt);

        let before_guard = capacity_custody_snapshot(&fixture.store);
        let error = fixture
            .store
            .close_capacity_incident(
                fixture.controller,
                CapacityCloseKind::ProgramTerminal,
                Some((invocation, 1)),
                Some(Uuid::new_v4()),
                fixture.now,
            )
            .expect_err("receipt guard mismatch must fail closed");
        assert!(
            error
                .to_string()
                .contains("capacity_close_program_guard_mismatch")
        );
        assert_eq!(capacity_custody_snapshot(&fixture.store), before_guard);

        let mut wrong_owner = crate::store::tests::make_test_session();
        wrong_owner.id = Uuid::new_v4();
        let wrong_owner_id = wrong_owner.id;
        fixture.store.insert_session(&wrong_owner).unwrap();
        fixture
            .store
            .conn
            .execute(
                "UPDATE model_invocations SET session_id=?1 WHERE id=?2",
                params![wrong_owner_id.to_string(), invocation.to_string()],
            )
            .unwrap();
        let before_owner = capacity_custody_snapshot(&fixture.store);
        let error = fixture
            .store
            .close_capacity_incident(
                fixture.controller,
                CapacityCloseKind::ProgramTerminal,
                Some((invocation, 1)),
                None,
                fixture.now,
            )
            .expect_err("corrupt receipt ownership must fail closed");
        assert!(
            error
                .to_string()
                .contains("capacity_close_receipt_source_mismatch")
        );
        assert_eq!(capacity_custody_snapshot(&fixture.store), before_owner);
    }
}
