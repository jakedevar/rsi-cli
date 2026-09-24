//! Durable finite sweep state for terminal sandbox target reclamation.

use super::{Store, sandbox_custody::TerminalCustodyReclaimCandidate};
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

const MAX_PAGE_SIZE: u32 =
    rsi_common::sandbox_storage::SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MAX;
const SWEEP_BUSY_TIMEOUT: Duration = Duration::from_millis(50);

// RSI-RELEASED-MIGRATION-BEGIN: v119-target-reclaim-sweep-fingerprints
pub(crate) const V118_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:391bd93dad3ced9c8663eedc948c39f9fd01f07a81fd34cbafc7e5cb21040e26";
pub(crate) const V119_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:0682d2612479d70e501a1c500694cb80b7c7add11aa62de7533ee17792809e33";
// RSI-RELEASED-MIGRATION-END: v119-target-reclaim-sweep-fingerprints

// RSI-RELEASED-MIGRATION-BEGIN: v119-target-reclaim-sweep-catalog
pub(crate) const V119_CATALOG_OBJECTS: [(&str, &str); 19] = [
    ("index", "idx_sandbox_custody_roots_reclaim_owner_v119"),
    ("index", "idx_sandbox_custody_roots_repository_state_v119"),
    (
        "index",
        "idx_sandbox_target_reclaim_intent_events_identity_v119",
    ),
    ("index", "idx_sandbox_target_reclaim_intents_state_v119"),
    ("index", "idx_sessions_terminal_reclaim_page_v119"),
    ("table", "sandbox_reclaim_sweeps"),
    ("table", "sandbox_target_reclaim_intent_events"),
    ("table", "sandbox_target_reclaim_intent_sweeps"),
    ("table", "sandbox_target_reclaim_intents"),
    (
        "trigger",
        "sandbox_target_reclaim_intent_events_v119_exact_insert",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intent_events_v119_no_delete",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intent_events_v119_no_update",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intent_sweeps_v119_forward_update",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intent_sweeps_v119_no_delete",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intents_v119_forward_update",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intents_v119_identity_immutable",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intents_v119_terminal_delete",
    ),
    ("trigger", "sandbox_reclaim_sweeps_v119_forward_update"),
    ("trigger", "sandbox_reclaim_sweeps_v119_no_delete"),
];

pub(crate) fn install_v119_table_and_row(tx: &Transaction<'_>, now: &str) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE sandbox_reclaim_sweeps (
            sweep_id INTEGER PRIMARY KEY NOT NULL CHECK(sweep_id=1),
            schema_version INTEGER NOT NULL CHECK(schema_version=1),
            cycle INTEGER NOT NULL CHECK(cycle>=0),
            after_updated_at TEXT
                CHECK(after_updated_at IS NULL OR rsi_rfc3339_is_valid(after_updated_at)),
            after_session_id TEXT
                CHECK(after_session_id IS NULL OR rsi_uuid_is_canonical(after_session_id)),
            upper_updated_at TEXT
                CHECK(upper_updated_at IS NULL OR rsi_rfc3339_is_valid(upper_updated_at)),
            upper_session_id TEXT
                CHECK(upper_session_id IS NULL OR rsi_uuid_is_canonical(upper_session_id)),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            CHECK((after_updated_at IS NULL)=(after_session_id IS NULL)),
            CHECK((upper_updated_at IS NULL)=(upper_session_id IS NULL)),
            CHECK(upper_updated_at IS NOT NULL OR after_updated_at IS NULL),
            CHECK(after_updated_at IS NULL OR after_updated_at<upper_updated_at
                OR (after_updated_at=upper_updated_at AND after_session_id<=upper_session_id))
        );
        CREATE TABLE sandbox_target_reclaim_intents (
            schedule_id INTEGER PRIMARY KEY AUTOINCREMENT,
            custody_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(custody_id)),
            generation INTEGER NOT NULL CHECK(generation>0),
            allocation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(allocation_id)),
            bucket INTEGER NOT NULL CHECK(bucket BETWEEN 0 AND 255),
            slot_name TEXT NOT NULL
                CHECK(slot_name='v3_' || custody_id || '_' || CAST(generation AS TEXT)),
            payload_name TEXT NOT NULL CHECK(payload_name='payload'),
            expected_device INTEGER NOT NULL CHECK(expected_device>=0),
            expected_inode INTEGER NOT NULL CHECK(expected_inode>0),
            state TEXT NOT NULL CHECK(state IN ('Prepared','Staged','Deleting')),
            row_version INTEGER NOT NULL CHECK(row_version>0),
            prepared_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(prepared_at)),
            staged_at TEXT CHECK(staged_at IS NULL OR rsi_rfc3339_nanos_is_canonical(staged_at)),
            deleting_at TEXT
                CHECK(deleting_at IS NULL OR rsi_rfc3339_nanos_is_canonical(deleting_at)),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            UNIQUE(custody_id,generation),
            UNIQUE(bucket,slot_name),
            FOREIGN KEY(custody_id)
                REFERENCES sandbox_custody_roots(custody_id)
                ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            CHECK((state='Prepared' AND row_version=1 AND staged_at IS NULL AND deleting_at IS NULL)
               OR (state='Staged' AND row_version>=2 AND staged_at IS NOT NULL AND deleting_at IS NULL)
               OR (state='Deleting' AND row_version>=3 AND staged_at IS NOT NULL
                   AND deleting_at IS NOT NULL)),
            CHECK(updated_at>=prepared_at
              AND (staged_at IS NULL OR staged_at>=prepared_at)
              AND (deleting_at IS NULL OR deleting_at>=staged_at))
        );
        CREATE TABLE sandbox_target_reclaim_intent_sweeps (
            sweep_id INTEGER PRIMARY KEY NOT NULL CHECK(sweep_id=1),
            schema_version INTEGER NOT NULL CHECK(schema_version=1),
            cycle INTEGER NOT NULL CHECK(cycle>=0),
            after_schedule_id INTEGER CHECK(after_schedule_id>0),
            upper_schedule_id INTEGER CHECK(upper_schedule_id>0),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            CHECK(upper_schedule_id IS NOT NULL OR after_schedule_id IS NULL),
            CHECK(after_schedule_id IS NULL OR after_schedule_id<=upper_schedule_id)
        );
        CREATE TABLE sandbox_target_reclaim_intent_events (
            event_id INTEGER PRIMARY KEY AUTOINCREMENT,
            schedule_id INTEGER NOT NULL CHECK(schedule_id>0),
            custody_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(custody_id)),
            generation INTEGER NOT NULL CHECK(generation>0),
            allocation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(allocation_id)),
            bucket INTEGER NOT NULL CHECK(bucket BETWEEN 0 AND 255),
            slot_name TEXT NOT NULL
                CHECK(slot_name='v3_' || custody_id || '_' || CAST(generation AS TEXT)),
            payload_name TEXT NOT NULL CHECK(payload_name='payload'),
            expected_device INTEGER NOT NULL CHECK(expected_device>=0),
            expected_inode INTEGER NOT NULL CHECK(expected_inode>0),
            terminal_state TEXT NOT NULL CHECK(terminal_state IN ('Completed','Abandoned')),
            reason TEXT NOT NULL CHECK(length(reason) BETWEEN 1 AND 96),
            final_row_version INTEGER NOT NULL CHECK(final_row_version>0),
            recorded_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(recorded_at)),
            UNIQUE(schedule_id),
            UNIQUE(custody_id,generation)
        );",
    )?;
    tx.execute(
        "INSERT INTO sandbox_reclaim_sweeps(
            sweep_id,schema_version,cycle,after_updated_at,after_session_id,
            upper_updated_at,upper_session_id,updated_at)
         VALUES(1,1,0,NULL,NULL,NULL,NULL,?1)",
        [now],
    )?;
    tx.execute(
        "INSERT INTO sandbox_target_reclaim_intent_sweeps(
            sweep_id,schema_version,cycle,after_schedule_id,upper_schedule_id,updated_at)
         VALUES(1,1,0,NULL,NULL,?1)",
        [now],
    )?;
    Ok(())
}

pub(crate) fn install_v119_indexes(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE INDEX idx_sessions_terminal_reclaim_page_v119
            ON sessions(updated_at,id)
            WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted');
        CREATE INDEX idx_sandbox_custody_roots_reclaim_owner_v119
            ON sandbox_custody_roots(owner_session_id,generation,custody_id)
            WHERE state='live' AND validation_state='verified'
              AND reserved_effects=0 AND active_effects=0;
        CREATE INDEX idx_sandbox_custody_roots_repository_state_v119
            ON sandbox_custody_roots(repository_identity,state,custody_id,generation);
        CREATE INDEX idx_sandbox_target_reclaim_intents_state_v119
            ON sandbox_target_reclaim_intents(state,schedule_id);
        CREATE INDEX idx_sandbox_target_reclaim_intent_events_identity_v119
            ON sandbox_target_reclaim_intent_events(custody_id,generation,event_id);",
    )?;
    Ok(())
}

pub(crate) fn install_v119_triggers(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TRIGGER sandbox_reclaim_sweeps_v119_forward_update
        BEFORE UPDATE ON sandbox_reclaim_sweeps
        WHEN NEW.sweep_id IS NOT OLD.sweep_id
          OR NEW.schema_version IS NOT OLD.schema_version
          OR NEW.cycle<OLD.cycle OR NEW.cycle>OLD.cycle+1
          OR NEW.updated_at<OLD.updated_at
          OR CASE
             WHEN NEW.cycle=OLD.cycle THEN CASE WHEN
                OLD.upper_updated_at IS NOT NULL
                AND (
                    (NEW.upper_updated_at IS OLD.upper_updated_at
                     AND NEW.upper_session_id IS OLD.upper_session_id
                     AND NEW.after_updated_at IS NOT NULL
                     AND (OLD.after_updated_at IS NULL
                          OR NEW.after_updated_at>OLD.after_updated_at
                          OR (NEW.after_updated_at=OLD.after_updated_at
                              AND NEW.after_session_id>OLD.after_session_id)))
                    OR
                    (NEW.upper_updated_at IS NULL AND NEW.upper_session_id IS NULL
                     AND NEW.after_updated_at IS NULL AND NEW.after_session_id IS NULL)
                ) THEN 0 ELSE 1 END
             WHEN NEW.cycle=OLD.cycle+1 THEN CASE WHEN
                OLD.upper_updated_at IS NULL AND OLD.upper_session_id IS NULL
                AND OLD.after_updated_at IS NULL AND OLD.after_session_id IS NULL
                THEN 0 ELSE 1 END
             ELSE 1 END
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim sweep is forward-only'); END;
        CREATE TRIGGER sandbox_reclaim_sweeps_v119_no_delete
        BEFORE DELETE ON sandbox_reclaim_sweeps
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim sweep is retained'); END;

        CREATE TRIGGER sandbox_target_reclaim_intents_v119_identity_immutable
        BEFORE UPDATE ON sandbox_target_reclaim_intents
        WHEN NEW.schedule_id!=OLD.schedule_id
          OR NEW.custody_id!=OLD.custody_id OR NEW.generation!=OLD.generation
          OR NEW.allocation_id!=OLD.allocation_id OR NEW.bucket!=OLD.bucket
          OR NEW.slot_name!=OLD.slot_name OR NEW.payload_name!=OLD.payload_name
          OR NEW.expected_device!=OLD.expected_device OR NEW.expected_inode!=OLD.expected_inode
          OR NEW.prepared_at!=OLD.prepared_at
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent identity is immutable'); END;

        CREATE TRIGGER sandbox_target_reclaim_intents_v119_forward_update
        BEFORE UPDATE ON sandbox_target_reclaim_intents
        WHEN NEW.row_version!=OLD.row_version+1 OR NEW.updated_at<OLD.updated_at
          OR NOT ((OLD.state='Prepared' AND NEW.state='Staged'
                   AND OLD.staged_at IS NULL AND NEW.staged_at IS NOT NULL
                   AND NEW.deleting_at IS NULL)
              OR (OLD.state='Staged' AND NEW.state='Deleting'
                   AND NEW.staged_at=OLD.staged_at AND OLD.deleting_at IS NULL
                   AND NEW.deleting_at IS NOT NULL))
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent transition is not forward'); END;

        CREATE TRIGGER sandbox_target_reclaim_intents_v119_terminal_delete
        BEFORE DELETE ON sandbox_target_reclaim_intents
        WHEN NOT EXISTS (
            SELECT 1 FROM sandbox_target_reclaim_intent_events e
            WHERE e.schedule_id=OLD.schedule_id
              AND e.custody_id=OLD.custody_id AND e.generation=OLD.generation
              AND e.final_row_version=OLD.row_version
              AND ((OLD.state='Deleting' AND e.terminal_state='Completed')
                   OR (OLD.state='Prepared' AND e.terminal_state='Abandoned'))
        )
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent requires terminal evidence'); END;

        CREATE TRIGGER sandbox_target_reclaim_intent_sweeps_v119_forward_update
        BEFORE UPDATE ON sandbox_target_reclaim_intent_sweeps
        WHEN NEW.sweep_id IS NOT OLD.sweep_id
          OR NEW.schema_version IS NOT OLD.schema_version
          OR NEW.cycle<OLD.cycle OR NEW.cycle>OLD.cycle+1
          OR NEW.updated_at<OLD.updated_at
          OR CASE
             WHEN NEW.cycle=OLD.cycle THEN CASE WHEN
                OLD.upper_schedule_id IS NOT NULL
                AND ((NEW.upper_schedule_id=OLD.upper_schedule_id
                      AND NEW.after_schedule_id IS NOT NULL
                      AND (OLD.after_schedule_id IS NULL
                           OR NEW.after_schedule_id>OLD.after_schedule_id))
                     OR (NEW.upper_schedule_id IS NULL
                         AND NEW.after_schedule_id IS NULL))
                THEN 0 ELSE 1 END
             WHEN NEW.cycle=OLD.cycle+1 THEN CASE WHEN
                OLD.upper_schedule_id IS NULL AND OLD.after_schedule_id IS NULL
                THEN 0 ELSE 1 END
             ELSE 1 END
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent sweep is forward-only'); END;
        CREATE TRIGGER sandbox_target_reclaim_intent_sweeps_v119_no_delete
        BEFORE DELETE ON sandbox_target_reclaim_intent_sweeps
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent sweep is retained'); END;

        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_exact_insert
        BEFORE INSERT ON sandbox_target_reclaim_intent_events
        WHEN NOT EXISTS (
            SELECT 1 FROM sandbox_target_reclaim_intents i
            WHERE i.schedule_id=NEW.schedule_id AND i.custody_id=NEW.custody_id
              AND i.generation=NEW.generation AND i.allocation_id=NEW.allocation_id
              AND i.bucket=NEW.bucket AND i.slot_name=NEW.slot_name
              AND i.payload_name=NEW.payload_name
              AND i.expected_device=NEW.expected_device
              AND i.expected_inode=NEW.expected_inode
              AND i.row_version=NEW.final_row_version
              AND ((i.state='Deleting' AND NEW.terminal_state='Completed')
                   OR (i.state='Prepared' AND NEW.terminal_state='Abandoned'))
        )
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence identity mismatch'); END;

        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_no_update
        BEFORE UPDATE ON sandbox_target_reclaim_intent_events
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence is immutable'); END;
        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_no_delete
        BEFORE DELETE ON sandbox_target_reclaim_intent_events
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence is retained'); END;",
    )?;
    Ok(())
}

pub(crate) fn validate_v119_catalog(connection: &Connection) -> Result<()> {
    for (kind, name) in V119_CATALOG_OBJECTS {
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type=?1 AND name=?2 AND sql IS NOT NULL",
            params![kind, name],
            |row| row.get(0),
        )?;
        if count != 1 {
            return Err(DaemonError::Store(format!(
                "V119 target reclaim catalog missing {kind} {name}"
            )));
        }
    }
    let rows: i64 = connection.query_row(
        "SELECT count(*) FROM sandbox_reclaim_sweeps
         WHERE sweep_id=1 AND schema_version=1 AND cycle>=0",
        [],
        |row| row.get(0),
    )?;
    if rows != 1 {
        return Err(DaemonError::Store(format!(
            "V119 target reclaim singleton row mismatch: {rows}"
        )));
    }
    let intent_sweep_rows: i64 = connection.query_row(
        "SELECT count(*) FROM sandbox_target_reclaim_intent_sweeps
         WHERE sweep_id=1 AND schema_version=1 AND cycle>=0",
        [],
        |row| row.get(0),
    )?;
    if intent_sweep_rows != 1 {
        return Err(DaemonError::Store(format!(
            "V119 target reclaim intent singleton row mismatch: {intent_sweep_rows}"
        )));
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v119-target-reclaim-sweep-catalog

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetReclaimSweepV119MigrationFault {
    AfterPreflight,
    AfterTableAndRow,
    AfterIndexes,
    AfterTriggers,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

#[cfg(test)]
impl TargetReclaimSweepV119MigrationFault {
    const ALL: [Self; 7] = [
        Self::AfterPreflight,
        Self::AfterTableAndRow,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static MIGRATION_FAULT: std::cell::Cell<Option<TargetReclaimSweepV119MigrationFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn fail_next_v119_migration(fault: TargetReclaimSweepV119MigrationFault) {
    MIGRATION_FAULT.with(|slot| slot.set(Some(fault)));
}

#[cfg(test)]
pub(crate) fn migration_fault(fault: TargetReclaimSweepV119MigrationFault) -> Result<()> {
    let injected = MIGRATION_FAULT.with(|slot| {
        if slot.get() == Some(fault) {
            slot.set(None);
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V119 target reclaim migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn migration_fault(_: TargetReclaimSweepV119MigrationFault) -> Result<()> {
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetReclaimIntentState {
    Prepared,
    Staged,
    Deleting,
}

impl TargetReclaimIntentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "Prepared",
            Self::Staged => "Staged",
            Self::Deleting => "Deleting",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "Prepared" => Ok(Self::Prepared),
            "Staged" => Ok(Self::Staged),
            "Deleting" => Ok(Self::Deleting),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TargetReclaimIntent {
    pub schedule_id: u64,
    pub custody_id: Uuid,
    pub generation: u64,
    pub allocation_id: Uuid,
    pub bucket: u8,
    pub slot_name: String,
    pub expected_device: u64,
    pub expected_inode: u64,
    pub state: TargetReclaimIntentState,
    pub row_version: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TargetReclaimIntentCounts {
    pub prepared: u32,
    pub staged: u32,
    pub deleting: u32,
    pub completed: u32,
    pub abandoned: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetReclaimIntentTerminalState {
    Completed,
    Abandoned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TargetReclaimIntentEvent {
    pub schedule_id: u64,
    pub custody_id: Uuid,
    pub generation: u64,
    pub allocation_id: Uuid,
    pub bucket: u8,
    pub slot_name: String,
    pub expected_device: u64,
    pub expected_inode: u64,
    pub terminal_state: TargetReclaimIntentTerminalState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PrepareTargetReclaimIntentResult {
    Active(TargetReclaimIntent),
    Terminal(TargetReclaimIntentEvent),
    SuccessorReservationPending,
}

#[cfg(test)]
impl PrepareTargetReclaimIntentResult {
    fn active(self) -> TargetReclaimIntent {
        match self {
            Self::Active(intent) => intent,
            Self::Terminal(_) | Self::SuccessorReservationPending => {
                panic!("new target reclaim fixture was not prepared")
            }
        }
    }
}

impl TargetReclaimIntentTerminalState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "Completed",
            Self::Abandoned => "Abandoned",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TargetReclaimIntentSweepEvidence {
    pub cycle_before: u64,
    pub cycle_after: u64,
    pub cursor_before: Option<u64>,
    pub cursor_after: Option<u64>,
    pub upper_bound: Option<u64>,
    pub page_key_digest: String,
    pub reserved: bool,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TargetReclaimIntentSweepPage {
    pub intents: Vec<TargetReclaimIntent>,
    pub has_more: bool,
    pub evidence: TargetReclaimIntentSweepEvidence,
}

pub(crate) fn target_reclaim_bucket(custody_id: Uuid, generation: u64) -> u8 {
    let mut digest = Sha256::new();
    digest.update(b"rsi.target-reclaim-bucket.v3\0");
    digest.update(custody_id.as_bytes());
    digest.update(generation.to_be_bytes());
    digest.finalize()[0]
}

pub(crate) fn target_reclaim_slot_name(custody_id: Uuid, generation: u64) -> String {
    format!("v3_{custody_id}_{generation}")
}

impl Store {
    pub(crate) fn target_reclaim_intent(
        &self,
        custody_id: Uuid,
        generation: u64,
    ) -> Result<Option<TargetReclaimIntent>> {
        load_target_reclaim_intent(&self.conn, custody_id, generation)
    }

    pub(crate) fn prepared_target_reclaim_owner(
        &self,
        intent: &TargetReclaimIntent,
    ) -> Result<Option<Uuid>> {
        if intent.state != TargetReclaimIntentState::Prepared {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT s.id
                 FROM sandbox_custody_roots r
                 JOIN sessions s ON s.id=r.owner_session_id
                   AND s.sandbox_custody_id=r.custody_id
                 WHERE r.custody_id=?1 AND r.generation=?2 AND r.allocation_id=?3
                   AND r.state='live' AND r.validation_state='verified'
                   AND r.validated_generation=r.generation
                   AND r.reserved_effects=0 AND r.active_effects=0
                   AND s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                   AND s.working_dir=r.canonical_repo_dir
                   AND s.sandbox_kind='GitWorktree'
                   AND s.sandbox_root=r.sandbox_root
                   AND s.sandbox_branch=r.sandbox_branch
                   AND s.sandbox_cleanup_state='Live'",
                params![
                    intent.custody_id.to_string(),
                    sqlite_u64(intent.generation, "target reclaim generation")?,
                    intent.allocation_id.to_string(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| {
                Uuid::parse_str(&value)
                    .map_err(|_| DaemonError::Store("target reclaim owner UUID is invalid".into()))
            })
            .transpose()
    }

    pub(crate) fn prepare_target_reclaim_intent(
        &self,
        custody_id: Uuid,
        generation: u64,
        expected_device: u64,
        expected_inode: u64,
    ) -> Result<PrepareTargetReclaimIntentResult> {
        if generation == 0 || expected_inode == 0 {
            return Err(DaemonError::InvalidParam(
                "target reclaim intent identity is invalid".into(),
            ));
        }
        let expected_device = i64::try_from(expected_device)
            .map_err(|_| DaemonError::Store("target reclaim device exceeds SQLite".into()))?;
        let expected_inode = i64::try_from(expected_inode)
            .map_err(|_| DaemonError::Store("target reclaim inode exceeds SQLite".into()))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(event) = load_target_reclaim_intent_event(&tx, custody_id, generation)? {
            tx.commit()?;
            return Ok(PrepareTargetReclaimIntentResult::Terminal(event));
        }
        let allocation_id = tx
            .query_row(
                "SELECT r.allocation_id
                 FROM sandbox_custody_roots r
                 JOIN sessions s ON s.id=r.owner_session_id
                   AND s.sandbox_custody_id=r.custody_id
                 WHERE r.custody_id=?1 AND r.generation=?2 AND r.state='live'
                   AND r.validation_state='verified'
                   AND r.validated_generation=r.generation
                   AND r.reserved_effects=0 AND r.active_effects=0
                   AND s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                   AND s.sandbox_cleanup_state='Live'",
                params![
                    custody_id.to_string(),
                    sqlite_u64(generation, "target reclaim generation")?
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::Store("target reclaim prepare lost custody fence".into())
            })?;
        let allocation_id = Uuid::parse_str(&allocation_id)
            .map_err(|_| DaemonError::Store("target reclaim allocation UUID is invalid".into()))?;
        // A retry or rotation can persist a Starting successor before custody
        // bind. Its reservation wins over a later reclaim gate in this same
        // SQLite writer order, so bind cannot strand an admitted successor.
        let successor_pending: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sandbox_custody_roots r
                JOIN sessions child ON child.continued_from=r.owner_session_id
                WHERE r.custody_id=?1 AND r.generation=?2
                  AND child.status='Starting' AND child.sandbox_custody_id IS NULL
                  AND child.sandbox_kind='GitWorktree'
                  AND child.sandbox_root=r.sandbox_root
                  AND child.sandbox_branch=r.sandbox_branch
                  AND child.sandbox_cleanup_state='Live'
            )",
            params![
                custody_id.to_string(),
                sqlite_u64(generation, "target reclaim generation")?
            ],
            |row| row.get(0),
        )?;
        if successor_pending {
            tx.commit()?;
            return Ok(PrepareTargetReclaimIntentResult::SuccessorReservationPending);
        }
        let bucket = target_reclaim_bucket(custody_id, generation);
        let slot_name = target_reclaim_slot_name(custody_id, generation);
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        tx.execute(
            "INSERT OR IGNORE INTO sandbox_target_reclaim_intents(
                custody_id,generation,allocation_id,bucket,slot_name,payload_name,
                expected_device,expected_inode,state,row_version,prepared_at,staged_at,
                deleting_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,'payload',?6,?7,'Prepared',1,?8,NULL,NULL,?8)",
            params![
                custody_id.to_string(),
                sqlite_u64(generation, "target reclaim generation")?,
                allocation_id.to_string(),
                i64::from(bucket),
                slot_name,
                expected_device,
                expected_inode,
                now,
            ],
        )?;
        let intent = load_target_reclaim_intent(&tx, custody_id, generation)?
            .ok_or_else(|| DaemonError::Store("target reclaim intent insert was lost".into()))?;
        if intent.allocation_id != allocation_id
            || intent.bucket != bucket
            || intent.slot_name != target_reclaim_slot_name(custody_id, generation)
            || intent.expected_device != expected_device as u64
            || intent.expected_inode != expected_inode as u64
        {
            return Err(DaemonError::Store(
                "target reclaim intent identity conflicts with durable row".into(),
            ));
        }
        tx.commit()?;
        Ok(PrepareTargetReclaimIntentResult::Active(intent))
    }

    pub(crate) fn mark_target_reclaim_staged(
        &self,
        intent: &TargetReclaimIntent,
    ) -> Result<TargetReclaimIntent> {
        advance_target_reclaim_intent(
            &self.conn,
            intent,
            TargetReclaimIntentState::Prepared,
            TargetReclaimIntentState::Staged,
        )
    }

    pub(crate) fn mark_target_reclaim_deleting(
        &self,
        intent: &TargetReclaimIntent,
    ) -> Result<TargetReclaimIntent> {
        advance_target_reclaim_intent(
            &self.conn,
            intent,
            TargetReclaimIntentState::Staged,
            TargetReclaimIntentState::Deleting,
        )
    }

    pub(crate) fn complete_target_reclaim_intent(
        &self,
        intent: &TargetReclaimIntent,
    ) -> Result<()> {
        settle_target_reclaim_intent(
            &self.conn,
            intent,
            TargetReclaimIntentState::Deleting,
            TargetReclaimIntentTerminalState::Completed,
            "durable_namespace_absent",
        )
    }

    pub(crate) fn abandon_target_reclaim_intent(
        &self,
        intent: &TargetReclaimIntent,
        reason: &'static str,
    ) -> Result<()> {
        if reason.is_empty() || reason.len() > 96 || !reason.is_ascii() {
            return Err(DaemonError::InvalidParam(
                "target reclaim abandonment reason is invalid".into(),
            ));
        }
        settle_target_reclaim_intent(
            &self.conn,
            intent,
            TargetReclaimIntentState::Prepared,
            TargetReclaimIntentTerminalState::Abandoned,
            reason,
        )
    }

    pub(crate) fn reserve_target_reclaim_intent_page(
        &self,
        limit: u32,
    ) -> Result<TargetReclaimIntentSweepPage> {
        validate_limit(limit)?;
        with_sweep_busy_timeout(&self.conn, || {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let state = load_intent_sweep_state(&tx)?;
            let selected = select_intent_page(&tx, &state, limit, true)?;
            persist_intent_selection(&tx, &selected)?;
            tx.commit()?;
            Ok(selected.page)
        })
    }

    pub(crate) fn preview_target_reclaim_intent_page(
        &self,
        limit: u32,
    ) -> Result<TargetReclaimIntentSweepPage> {
        validate_limit(limit)?;
        with_sweep_busy_timeout(&self.conn, || {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            let state = load_intent_sweep_state(&tx)?;
            let page = select_intent_page(&tx, &state, limit, false)?.page;
            tx.commit()?;
            Ok(page)
        })
    }

    /// Read a fair page of gates for the prompt recovery loop. Recovery
    /// revalidates each row before touching the filesystem, so this cursor
    /// is deliberately advisory and does not mutate the durable sweep.
    pub(crate) fn prepared_target_reclaim_intent_page(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<Vec<TargetReclaimIntent>> {
        validate_limit(limit)?;
        let read = |after: u64| -> Result<Vec<TargetReclaimIntent>> {
            let mut statement = self.conn.prepare(
                "SELECT schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                        expected_device,expected_inode,state,row_version
                 FROM sandbox_target_reclaim_intents
                 WHERE state='Prepared' AND schedule_id>?1
                 ORDER BY schedule_id ASC LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![
                        sqlite_u64(after, "target reclaim schedule ID")?,
                        i64::from(limit)
                    ],
                    decode_target_reclaim_intent,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Into::into)
        };
        let page = read(after)?;
        if page.is_empty() && after != 0 {
            read(0)
        } else {
            Ok(page)
        }
    }

    pub(crate) fn target_reclaim_intent_counts(&self) -> Result<TargetReclaimIntentCounts> {
        self.conn
            .query_row(
                "SELECT
                    (SELECT count(*) FROM sandbox_target_reclaim_intents WHERE state='Prepared'),
                    (SELECT count(*) FROM sandbox_target_reclaim_intents WHERE state='Staged'),
                    (SELECT count(*) FROM sandbox_target_reclaim_intents WHERE state='Deleting'),
                    (SELECT count(*) FROM sandbox_target_reclaim_intent_events
                     WHERE terminal_state='Completed'),
                    (SELECT count(*) FROM sandbox_target_reclaim_intent_events
                     WHERE terminal_state='Abandoned')",
                [],
                |row| {
                    Ok(TargetReclaimIntentCounts {
                        prepared: u32::try_from(row.get::<_, i64>(0)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        staged: u32::try_from(row.get::<_, i64>(1)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        deleting: u32::try_from(row.get::<_, i64>(2)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        completed: u32::try_from(row.get::<_, i64>(3)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        abandoned: u32::try_from(row.get::<_, i64>(4)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                    })
                },
            )
            .map_err(Into::into)
    }
}

fn advance_target_reclaim_intent(
    connection: &Connection,
    intent: &TargetReclaimIntent,
    expected: TargetReclaimIntentState,
    next: TargetReclaimIntentState,
) -> Result<TargetReclaimIntent> {
    if intent.state != expected {
        return Err(DaemonError::Store(
            "target reclaim intent has unexpected source state".into(),
        ));
    }
    let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let transition_column = match next {
        TargetReclaimIntentState::Staged => "staged_at",
        TargetReclaimIntentState::Deleting => "deleting_at",
        TargetReclaimIntentState::Prepared => {
            return Err(DaemonError::Store(
                "target reclaim intent cannot transition backward".into(),
            ));
        }
    };
    let sql = format!(
        "UPDATE sandbox_target_reclaim_intents
         SET state=?4,row_version=row_version+1,
             {transition_column}=CASE WHEN updated_at>?5 THEN updated_at ELSE ?5 END,
             updated_at=CASE WHEN updated_at>?5 THEN updated_at ELSE ?5 END
         WHERE custody_id=?1 AND generation=?2 AND state=?3 AND row_version=?6"
    );
    let changed = tx.execute(
        &sql,
        params![
            intent.custody_id.to_string(),
            sqlite_u64(intent.generation, "target reclaim generation")?,
            expected.as_str(),
            next.as_str(),
            now,
            sqlite_u64(intent.row_version, "target reclaim row version")?,
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "target reclaim intent transition lost row-version fence".into(),
        ));
    }
    let advanced = load_target_reclaim_intent(&tx, intent.custody_id, intent.generation)?
        .ok_or_else(|| DaemonError::Store("target reclaim intent disappeared".into()))?;
    tx.commit()?;
    Ok(advanced)
}

fn settle_target_reclaim_intent(
    connection: &Connection,
    intent: &TargetReclaimIntent,
    expected: TargetReclaimIntentState,
    terminal: TargetReclaimIntentTerminalState,
    reason: &str,
) -> Result<()> {
    if intent.state != expected {
        return Err(DaemonError::Store(
            "target reclaim settlement has unexpected source state".into(),
        ));
    }
    let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    let current = load_target_reclaim_intent(&tx, intent.custody_id, intent.generation)?;
    if current.as_ref() != Some(intent) {
        return Err(DaemonError::Store(
            "target reclaim settlement lost row-version fence".into(),
        ));
    }
    tx.execute(
        "INSERT INTO sandbox_target_reclaim_intent_events(
            schedule_id,custody_id,generation,allocation_id,bucket,slot_name,payload_name,
            expected_device,expected_inode,terminal_state,reason,final_row_version,recorded_at)
         VALUES(?1,?2,?3,?4,?5,?6,'payload',?7,?8,?9,?10,?11,?12)",
        params![
            sqlite_u64(intent.schedule_id, "target reclaim schedule ID")?,
            intent.custody_id.to_string(),
            sqlite_u64(intent.generation, "target reclaim generation")?,
            intent.allocation_id.to_string(),
            i64::from(intent.bucket),
            intent.slot_name,
            i64::try_from(intent.expected_device)
                .map_err(|_| DaemonError::Store("target reclaim device exceeds SQLite".into()))?,
            i64::try_from(intent.expected_inode)
                .map_err(|_| DaemonError::Store("target reclaim inode exceeds SQLite".into()))?,
            terminal.as_str(),
            reason,
            sqlite_u64(intent.row_version, "target reclaim row version")?,
            Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        ],
    )?;
    let changed = tx.execute(
        "DELETE FROM sandbox_target_reclaim_intents
         WHERE schedule_id=?1 AND custody_id=?2 AND generation=?3
           AND state=?4 AND row_version=?5",
        params![
            sqlite_u64(intent.schedule_id, "target reclaim schedule ID")?,
            intent.custody_id.to_string(),
            sqlite_u64(intent.generation, "target reclaim generation")?,
            expected.as_str(),
            sqlite_u64(intent.row_version, "target reclaim row version")?,
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "target reclaim settlement lost row-version fence".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

fn load_target_reclaim_intent_event(
    connection: &Connection,
    custody_id: Uuid,
    generation: u64,
) -> Result<Option<TargetReclaimIntentEvent>> {
    connection
        .query_row(
            "SELECT schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                    expected_device,expected_inode,terminal_state
             FROM sandbox_target_reclaim_intent_events
             WHERE custody_id=?1 AND generation=?2",
            params![
                custody_id.to_string(),
                sqlite_u64(generation, "target reclaim generation")?
            ],
            decode_target_reclaim_intent_event,
        )
        .optional()
        .map_err(Into::into)
}

struct DecodedTargetReclaimIdentity {
    schedule_id: u64,
    custody_id: Uuid,
    generation: u64,
    allocation_id: Uuid,
    bucket: u8,
    slot_name: String,
    expected_device: u64,
    expected_inode: u64,
}

fn decode_target_reclaim_identity(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<DecodedTargetReclaimIdentity> {
    let uuid = |column, value: String| {
        Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    let identity = DecodedTargetReclaimIdentity {
        schedule_id: u64::try_from(row.get::<_, i64>(0)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        custody_id: uuid(1, row.get(1)?)?,
        generation: u64::try_from(row.get::<_, i64>(2)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        allocation_id: uuid(3, row.get(3)?)?,
        bucket: u8::try_from(row.get::<_, i64>(4)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        slot_name: row.get(5)?,
        expected_device: u64::try_from(row.get::<_, i64>(6)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        expected_inode: u64::try_from(row.get::<_, i64>(7)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
    };
    if identity.bucket != target_reclaim_bucket(identity.custody_id, identity.generation)
        || identity.slot_name != target_reclaim_slot_name(identity.custody_id, identity.generation)
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(identity)
}

fn decode_target_reclaim_intent_event(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<TargetReclaimIntentEvent> {
    let active = decode_target_reclaim_identity(row)?;
    let terminal_state = match row.get::<_, String>(8)?.as_str() {
        "Completed" => TargetReclaimIntentTerminalState::Completed,
        "Abandoned" => TargetReclaimIntentTerminalState::Abandoned,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(TargetReclaimIntentEvent {
        schedule_id: active.schedule_id,
        custody_id: active.custody_id,
        generation: active.generation,
        allocation_id: active.allocation_id,
        bucket: active.bucket,
        slot_name: active.slot_name,
        expected_device: active.expected_device,
        expected_inode: active.expected_inode,
        terminal_state,
    })
}

fn sqlite_u64(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| DaemonError::Store(format!("{label} exceeds SQLite")))
}

fn load_target_reclaim_intent(
    connection: &Connection,
    custody_id: Uuid,
    generation: u64,
) -> Result<Option<TargetReclaimIntent>> {
    connection
        .query_row(
            "SELECT schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                    expected_device,expected_inode,state,row_version
             FROM sandbox_target_reclaim_intents
             WHERE custody_id=?1 AND generation=?2",
            params![
                custody_id.to_string(),
                sqlite_u64(generation, "target reclaim generation")?
            ],
            decode_target_reclaim_intent,
        )
        .optional()
        .map_err(Into::into)
}

fn decode_target_reclaim_intent(row: &rusqlite::Row<'_>) -> rusqlite::Result<TargetReclaimIntent> {
    let identity = decode_target_reclaim_identity(row)?;
    Ok(TargetReclaimIntent {
        schedule_id: identity.schedule_id,
        custody_id: identity.custody_id,
        generation: identity.generation,
        allocation_id: identity.allocation_id,
        bucket: identity.bucket,
        slot_name: identity.slot_name,
        expected_device: identity.expected_device,
        expected_inode: identity.expected_inode,
        state: TargetReclaimIntentState::parse(&row.get::<_, String>(8)?)?,
        row_version: u64::try_from(row.get::<_, i64>(9)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
    })
}

#[derive(Clone, Copy, Debug)]
struct IntentSweepState {
    cycle: u64,
    after: Option<u64>,
    upper: Option<u64>,
}

struct SelectedIntentPage {
    page: TargetReclaimIntentSweepPage,
    persisted_cycle: u64,
    persisted_after: Option<u64>,
    persisted_upper: Option<u64>,
}

fn load_intent_sweep_state(connection: &Connection) -> Result<IntentSweepState> {
    connection
        .query_row(
            "SELECT cycle,after_schedule_id,upper_schedule_id
             FROM sandbox_target_reclaim_intent_sweeps
             WHERE sweep_id=1 AND schema_version=1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| DaemonError::Store("V119 target reclaim intent sweep row is missing".into()))
        .and_then(|(cycle, after, upper)| {
            Ok(IntentSweepState {
                cycle: u64::try_from(cycle).map_err(|_| {
                    DaemonError::Store("V119 target reclaim intent cycle is negative".into())
                })?,
                after: after
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| DaemonError::Store("invalid intent sweep cursor".into()))?,
                upper: upper
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| DaemonError::Store("invalid intent sweep upper bound".into()))?,
            })
        })
}

fn select_intent_page(
    connection: &Connection,
    state: &IntentSweepState,
    limit: u32,
    reserved: bool,
) -> Result<SelectedIntentPage> {
    let (cycle_after, upper) = match state.upper {
        Some(upper) => (state.cycle, Some(upper)),
        None => (
            state
                .cycle
                .checked_add(1)
                .ok_or_else(|| DaemonError::Store("target reclaim intent cycle overflow".into()))?,
            connection
                .query_row(
                    "SELECT max(schedule_id) FROM sandbox_target_reclaim_intents",
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )?
                .map(u64::try_from)
                .transpose()
                .map_err(|_| DaemonError::Store("invalid intent sweep upper bound".into()))?,
        ),
    };
    let intents = match upper {
        Some(upper) => select_intents_between(connection, state.after, upper, limit)?,
        None => Vec::new(),
    };
    let cursor_after = intents
        .last()
        .map(|intent| intent.schedule_id)
        .or(state.after);
    let has_more = match (cursor_after, upper) {
        (Some(after), Some(upper)) => connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_target_reclaim_intents
                           WHERE schedule_id>?1 AND schedule_id<=?2)",
            params![
                sqlite_u64(after, "target reclaim schedule ID")?,
                sqlite_u64(upper, "target reclaim schedule ID")?
            ],
            |row| row.get::<_, bool>(0),
        )?,
        _ => false,
    };
    let completing_active_cycle = state.upper.is_some() && intents.is_empty();
    let persisted_after = (!completing_active_cycle).then_some(cursor_after).flatten();
    let persisted_upper = (!completing_active_cycle).then_some(upper).flatten();
    let wrapped = completing_active_cycle || (state.upper.is_none() && upper.is_none());
    let digest = intent_page_digest(&intents);

    Ok(SelectedIntentPage {
        page: TargetReclaimIntentSweepPage {
            intents,
            has_more,
            evidence: TargetReclaimIntentSweepEvidence {
                cycle_before: state.cycle,
                cycle_after: if reserved { cycle_after } else { state.cycle },
                cursor_before: state.after,
                cursor_after: if reserved {
                    persisted_after
                } else {
                    state.after
                },
                upper_bound: upper,
                page_key_digest: digest,
                reserved,
                wrapped: reserved && wrapped,
            },
        },
        persisted_cycle: cycle_after,
        persisted_after,
        persisted_upper,
    })
}

fn select_intents_between(
    connection: &Connection,
    after: Option<u64>,
    upper: u64,
    limit: u32,
) -> Result<Vec<TargetReclaimIntent>> {
    let mut statement = connection.prepare(
        "SELECT schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                expected_device,expected_inode,state,row_version
         FROM sandbox_target_reclaim_intents
         WHERE schedule_id>?1 AND schedule_id<=?2
         ORDER BY schedule_id ASC LIMIT ?3",
    )?;
    statement
        .query_map(
            params![
                sqlite_u64(after.unwrap_or(0), "target reclaim schedule ID")?,
                sqlite_u64(upper, "target reclaim schedule ID")?,
                i64::from(limit),
            ],
            decode_target_reclaim_intent,
        )?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn persist_intent_selection(tx: &Transaction<'_>, selected: &SelectedIntentPage) -> Result<()> {
    let changed = tx.execute(
        "UPDATE sandbox_target_reclaim_intent_sweeps
         SET cycle=?1,after_schedule_id=?2,upper_schedule_id=?3,
             updated_at=CASE WHEN updated_at>?4 THEN updated_at ELSE ?4 END
         WHERE sweep_id=1 AND schema_version=1",
        params![
            sqlite_u64(selected.persisted_cycle, "target reclaim intent cycle")?,
            selected
                .persisted_after
                .map(|value| sqlite_u64(value, "target reclaim schedule ID"))
                .transpose()?,
            selected
                .persisted_upper
                .map(|value| sqlite_u64(value, "target reclaim schedule ID"))
                .transpose()?,
            Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "V119 target reclaim intent sweep reservation lost singleton row".into(),
        ));
    }
    Ok(())
}

fn intent_page_digest(intents: &[TargetReclaimIntent]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rsi.target-reclaim-intent-page.v1\0");
    for intent in intents {
        digest.update(intent.schedule_id.to_be_bytes());
        digest.update(intent.custody_id.as_bytes());
        digest.update(intent.generation.to_be_bytes());
        digest.update(intent.row_version.to_be_bytes());
    }
    format!("sha256:{:x}", digest.finalize())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalReclaimSweepKey {
    pub updated_at: String,
    pub session_id: Uuid,
}

impl From<&TerminalCustodyReclaimCandidate> for TerminalReclaimSweepKey {
    fn from(candidate: &TerminalCustodyReclaimCandidate) -> Self {
        Self {
            updated_at: candidate.updated_at.clone(),
            session_id: candidate.session_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalReclaimSweepEvidence {
    pub cycle_before: u64,
    pub cycle_after: u64,
    pub cursor_before: Option<TerminalReclaimSweepKey>,
    pub cursor_after: Option<TerminalReclaimSweepKey>,
    pub upper_bound: Option<TerminalReclaimSweepKey>,
    pub page_key_digest: String,
    pub inspected_terminal_rows: u32,
    pub custody_lookups: u32,
    pub reserved: bool,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalReclaimSweepPage {
    pub candidates: Vec<TerminalCustodyReclaimCandidate>,
    pub has_more: bool,
    pub evidence: TerminalReclaimSweepEvidence,
}

#[derive(Clone, Debug)]
struct SweepState {
    cycle: u64,
    after: Option<TerminalReclaimSweepKey>,
    upper: Option<TerminalReclaimSweepKey>,
}

impl Store {
    /// Atomically reserves one bounded page before any filesystem effect.
    /// Every inspected terminal key consumes its finite-sweep position even if
    /// custody filtering omits it. A crash can lose only this bounded raw page
    /// until the next cycle.
    pub(crate) fn reserve_terminal_reclaim_page(
        &self,
        limit: u32,
    ) -> Result<TerminalReclaimSweepPage> {
        validate_limit(limit)?;
        with_sweep_busy_timeout(&self.conn, || {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let state = load_state(&tx)?;
            let selected = select_page(&tx, &state, limit, true)?;
            persist_selection(&tx, &selected)?;
            tx.commit()?;
            Ok(selected.page)
        })
    }

    /// Computes the exact next page under one read transaction and performs no
    /// cursor write. Repeated previews therefore return identical evidence
    /// until terminal source rows or durable sweep state change.
    pub(crate) fn preview_terminal_reclaim_page(
        &self,
        limit: u32,
    ) -> Result<TerminalReclaimSweepPage> {
        validate_limit(limit)?;
        with_sweep_busy_timeout(&self.conn, || {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            let state = load_state(&tx)?;
            let page = select_page(&tx, &state, limit, false)?.page;
            tx.commit()?;
            Ok(page)
        })
    }
}

fn with_sweep_busy_timeout<T>(
    connection: &Connection,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let previous_millis =
        connection.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, u64>(0))?;
    connection.busy_timeout(SWEEP_BUSY_TIMEOUT)?;
    let result = operation();
    let restore = connection.busy_timeout(Duration::from_millis(previous_millis));
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
    }
}

fn validate_limit(limit: u32) -> Result<()> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(DaemonError::InvalidParam(format!(
            "target reclaim page limit must be within 1..={MAX_PAGE_SIZE}"
        )));
    }
    Ok(())
}

fn load_state(connection: &Connection) -> Result<SweepState> {
    connection
        .query_row(
            "SELECT cycle,after_updated_at,after_session_id,upper_updated_at,upper_session_id
             FROM sandbox_reclaim_sweeps WHERE sweep_id=1 AND schema_version=1",
            [],
            |row| {
                let cycle = row.get::<_, i64>(0)?;
                let after = parse_key(row.get(1)?, row.get(2)?, 1)?;
                let upper = parse_key(row.get(3)?, row.get(4)?, 3)?;
                Ok((cycle, after, upper))
            },
        )
        .optional()?
        .ok_or_else(|| DaemonError::Store("V119 target reclaim sweep row is missing".into()))
        .and_then(|(cycle, after, upper)| {
            let cycle = u64::try_from(cycle)
                .map_err(|_| DaemonError::Store("V119 target reclaim cycle is negative".into()))?;
            Ok(SweepState {
                cycle,
                after,
                upper,
            })
        })
}

fn parse_key(
    updated_at: Option<String>,
    session_id: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<TerminalReclaimSweepKey>> {
    match (updated_at, session_id) {
        (None, None) => Ok(None),
        (Some(updated_at), Some(session_id)) => Ok(Some(TerminalReclaimSweepKey {
            updated_at,
            session_id: Uuid::parse_str(&session_id).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    column,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        })),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

struct SelectedPage {
    page: TerminalReclaimSweepPage,
    persisted_cycle: u64,
    persisted_after: Option<TerminalReclaimSweepKey>,
    persisted_upper: Option<TerminalReclaimSweepKey>,
}

fn select_page(
    connection: &Connection,
    state: &SweepState,
    limit: u32,
    reserved: bool,
) -> Result<SelectedPage> {
    let (cycle_after, upper) = match &state.upper {
        Some(upper) => (state.cycle, Some(upper.clone())),
        None => (
            state
                .cycle
                .checked_add(1)
                .ok_or_else(|| DaemonError::Store("target reclaim cycle overflow".into()))?,
            select_upper_bound(connection)?,
        ),
    };

    let terminal_keys = match &upper {
        Some(upper) => select_terminal_keys(connection, state.after.as_ref(), upper, limit)?,
        None => Vec::new(),
    };
    let candidates = terminal_keys
        .iter()
        .map(|key| select_candidate_for_key(connection, key))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let has_more = terminal_keys
        .last()
        .is_some_and(|last| key_less(last, upper.as_ref().unwrap()));
    let cursor_after = terminal_keys
        .last()
        .cloned()
        .or_else(|| state.after.clone());
    let completing_active_cycle = state.upper.is_some() && terminal_keys.is_empty();
    let persisted_after = if completing_active_cycle {
        None
    } else {
        cursor_after.clone()
    };
    let persisted_upper = if completing_active_cycle {
        None
    } else {
        upper.clone()
    };
    let wrapped = completing_active_cycle || (state.upper.is_none() && upper.is_none());

    Ok(SelectedPage {
        page: TerminalReclaimSweepPage {
            evidence: TerminalReclaimSweepEvidence {
                cycle_before: state.cycle,
                cycle_after: if reserved { cycle_after } else { state.cycle },
                cursor_before: state.after.clone(),
                cursor_after: if reserved {
                    persisted_after.clone()
                } else {
                    state.after.clone()
                },
                upper_bound: upper,
                page_key_digest: page_key_digest(&terminal_keys),
                inspected_terminal_rows: terminal_keys.len() as u32,
                custody_lookups: terminal_keys.len() as u32,
                reserved,
                wrapped: reserved && wrapped,
            },
            candidates,
            has_more,
        },
        persisted_cycle: cycle_after,
        persisted_after,
        persisted_upper,
    })
}

const SELECT_UPPER_BOUND_SQL: &str = "SELECT updated_at,id FROM sessions
     WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
     ORDER BY updated_at DESC,id DESC LIMIT 1";

fn select_upper_bound(connection: &Connection) -> Result<Option<TerminalReclaimSweepKey>> {
    connection
        .query_row(SELECT_UPPER_BOUND_SQL, [], |row| {
            parse_key(Some(row.get(0)?), Some(row.get(1)?), 1).map(Option::unwrap)
        })
        .optional()
        .map_err(Into::into)
}

const SELECT_TERMINAL_KEYS_FROM_START_SQL: &str = "SELECT updated_at,id FROM sessions
     WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
       AND (updated_at,id)<=(?1,?2)
     ORDER BY updated_at ASC,id ASC LIMIT ?3";

const SELECT_TERMINAL_KEYS_AFTER_SQL: &str = "SELECT updated_at,id FROM sessions
     WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
       AND (updated_at,id)>(?1,?2)
       AND (updated_at,id)<=(?3,?4)
     ORDER BY updated_at ASC,id ASC LIMIT ?5";

fn select_terminal_keys(
    connection: &Connection,
    after: Option<&TerminalReclaimSweepKey>,
    upper: &TerminalReclaimSweepKey,
    limit: u32,
) -> Result<Vec<TerminalReclaimSweepKey>> {
    let map = |row: &rusqlite::Row<'_>| {
        parse_key(Some(row.get(0)?), Some(row.get(1)?), 1).map(Option::unwrap)
    };
    let mut statement;
    let rows = match after {
        Some(after) => {
            statement = connection.prepare(SELECT_TERMINAL_KEYS_AFTER_SQL)?;
            statement.query_map(
                params![
                    after.updated_at,
                    after.session_id.to_string(),
                    upper.updated_at,
                    upper.session_id.to_string(),
                    i64::from(limit)
                ],
                map,
            )?
        }
        None => {
            statement = connection.prepare(SELECT_TERMINAL_KEYS_FROM_START_SQL)?;
            statement.query_map(
                params![
                    upper.updated_at,
                    upper.session_id.to_string(),
                    i64::from(limit)
                ],
                map,
            )?
        }
    };
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

const SELECT_CANDIDATE_FOR_KEY_SQL: &str = "SELECT r.custody_id,r.generation,s.id,s.updated_at
     FROM sessions s
     JOIN sandbox_custody_roots r
       ON r.owner_session_id=s.id AND s.sandbox_custody_id=r.custody_id
     WHERE s.id=?1 AND s.updated_at=?2
       AND r.state='live' AND r.validation_state='verified'
       AND r.validated_generation=r.generation
       AND r.reserved_effects=0 AND r.active_effects=0";

fn select_candidate_for_key(
    connection: &Connection,
    key: &TerminalReclaimSweepKey,
) -> Result<Option<TerminalCustodyReclaimCandidate>> {
    connection
        .query_row(
            SELECT_CANDIDATE_FOR_KEY_SQL,
            params![key.session_id.to_string(), key.updated_at],
            |row| {
                let custody_id = row.get::<_, String>(0)?;
                let session_id = row.get::<_, String>(2)?;
                Ok(TerminalCustodyReclaimCandidate {
                    custody_id: Uuid::parse_str(&custody_id).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    generation: u64::try_from(row.get::<_, i64>(1)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Integer,
                            Box::new(error),
                        )
                    })?,
                    session_id: Uuid::parse_str(&session_id).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    updated_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn key_less(left: &TerminalReclaimSweepKey, right: &TerminalReclaimSweepKey) -> bool {
    (left.updated_at.as_str(), left.session_id) < (right.updated_at.as_str(), right.session_id)
}

fn persist_selection(tx: &Transaction<'_>, selected: &SelectedPage) -> Result<()> {
    let changed = tx.execute(
        "UPDATE sandbox_reclaim_sweeps
         SET cycle=?1,after_updated_at=?2,after_session_id=?3,
             upper_updated_at=?4,upper_session_id=?5,
             updated_at=CASE WHEN updated_at>?6 THEN updated_at ELSE ?6 END
         WHERE sweep_id=1 AND schema_version=1",
        params![
            i64::try_from(selected.persisted_cycle)
                .map_err(|_| DaemonError::Store("target reclaim cycle exceeds SQLite".into()))?,
            selected
                .persisted_after
                .as_ref()
                .map(|key| key.updated_at.as_str()),
            selected
                .persisted_after
                .as_ref()
                .map(|key| key.session_id.to_string()),
            selected
                .persisted_upper
                .as_ref()
                .map(|key| key.updated_at.as_str()),
            selected
                .persisted_upper
                .as_ref()
                .map(|key| key.session_id.to_string()),
            Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "V119 target reclaim sweep reservation lost singleton row".into(),
        ));
    }
    Ok(())
}

fn page_key_digest(keys: &[TerminalReclaimSweepKey]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rsi.target-reclaim-page.v1\0");
    for key in keys {
        for value in [key.updated_at.as_bytes(), key.session_id.as_bytes()] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
    }
    format!("sha256:{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        capacity_recovery,
        sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding},
        tests::{make_test_session, rewind_store_to_schema_version},
    };
    use rsi_common::types::{
        SandboxCleanupState, SandboxCustodyErrorCodeV1, SandboxCustodyTransitionV1, SandboxKind,
        SessionStatus,
    };
    use std::path::{Path, PathBuf};

    struct FixtureDirectory(PathBuf);

    impl FixtureDirectory {
        fn create(label: &str) -> Self {
            let base = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target"));
            let path = base
                .join("rsid-test-fixtures")
                .join(format!("{label}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create disk-backed V119 fixture");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).expect("remove isolated V119 fixture");
        }
    }

    fn fingerprint(connection: &Connection) -> String {
        let tx = connection.unchecked_transaction().unwrap();
        let result = capacity_recovery::v88_full_catalog_fingerprint(&tx).unwrap();
        tx.commit().unwrap();
        result
    }

    fn seed_candidate(store: &mut Store, fixture: &Path, ordinal: u32) -> Uuid {
        let mut session = make_test_session();
        session.id = Uuid::from_u128(u128::from(ordinal) + 1);
        session.status = SessionStatus::Completed;
        session.created_at = chrono::DateTime::parse_from_rfc3339(&format!(
            "2026-09-13T12:{:02}:00.000000000Z",
            ordinal
        ))
        .unwrap()
        .with_timezone(&chrono::Utc);
        session.updated_at = session.created_at;
        session.working_dir = fixture.join("repository");
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(fixture.join(format!("sandbox-{ordinal}")));
        session.sandbox_branch = Some(format!("rsi/v119-{ordinal}"));
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        let custody_id = Uuid::from_u128(u128::from(ordinal) + 10_000);
        store
            .insert_session_with_custody(
                &session,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id,
                    canonical_repo_dir: session.working_dir.to_string_lossy().into_owned(),
                    sandbox_root: session
                        .sandbox_root
                        .as_ref()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    sandbox_branch: session.sandbox_branch.clone().unwrap(),
                    repository_identity: "repo:v119-fixture".into(),
                    source_commit: "a".repeat(40),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .unwrap();
        session.id
    }

    fn seed_terminal_without_custody(store: &Store, fixture: &Path, ordinal: u32) -> Uuid {
        let mut session = make_test_session();
        session.id = Uuid::from_u128(u128::from(ordinal) + 100_000);
        session.status = SessionStatus::Completed;
        session.created_at = chrono::DateTime::parse_from_rfc3339("2026-09-13T11:00:00.000000000Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(i64::from(ordinal));
        session.updated_at = session.created_at;
        session.working_dir = fixture.join("repository");
        store.insert_session(&session).unwrap();
        session.id
    }

    #[test]
    fn terminal_custody_reclaim_candidates_feed_durable_sweep_and_survive_reopen() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-reopen");
        let database = fixture.path().join("reclaim.db");
        let mut store = Store::open(&database).unwrap();
        let ids = (0..5)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();

        let preview = store.preview_terminal_reclaim_page(2).unwrap();
        assert!(!preview.evidence.reserved);
        assert_eq!(preview.evidence.cycle_before, 0);
        assert_eq!(preview.evidence.cycle_after, 0);
        assert_eq!(
            preview.evidence.cursor_before,
            preview.evidence.cursor_after
        );
        assert!(!preview.evidence.wrapped);
        assert_eq!(
            preview
                .candidates
                .iter()
                .map(|candidate| candidate.session_id)
                .collect::<Vec<_>>(),
            ids[..2]
        );
        assert_eq!(store.preview_terminal_reclaim_page(2).unwrap(), preview);

        let reserved = store.reserve_terminal_reclaim_page(2).unwrap();
        assert!(reserved.evidence.reserved);
        assert_eq!(reserved.candidates, preview.candidates);
        drop(store);

        let reopened = Store::open(&database).unwrap();
        let second = reopened.reserve_terminal_reclaim_page(2).unwrap();
        assert_eq!(second.evidence.cycle_before, 1);
        assert_eq!(
            second
                .candidates
                .iter()
                .map(|candidate| candidate.session_id)
                .collect::<Vec<_>>(),
            ids[2..4]
        );
        let third = reopened.reserve_terminal_reclaim_page(2).unwrap();
        assert_eq!(third.candidates[0].session_id, ids[4]);
        assert!(!third.has_more);
        let wrap = reopened.reserve_terminal_reclaim_page(2).unwrap();
        assert!(wrap.candidates.is_empty());
        assert!(wrap.evidence.wrapped);
        let next_cycle = reopened.reserve_terminal_reclaim_page(2).unwrap();
        assert_eq!(next_cycle.evidence.cycle_before, 1);
        assert_eq!(next_cycle.evidence.cycle_after, 2);
        assert_eq!(next_cycle.candidates[0].session_id, ids[0]);
    }

    #[test]
    fn target_reclaim_sweep_upper_bound_defers_new_rows_and_forward_guards_reject_rewind() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-upper");
        let mut store = Store::open_in_memory().unwrap();
        seed_candidate(&mut store, fixture.path(), 0);
        seed_candidate(&mut store, fixture.path(), 1);
        let first = store.reserve_terminal_reclaim_page(1).unwrap();
        seed_candidate(&mut store, fixture.path(), 2);
        let second = store.reserve_terminal_reclaim_page(2).unwrap();
        assert_eq!(second.candidates.len(), 1);
        assert_eq!(second.evidence.upper_bound, first.evidence.upper_bound);

        assert!(
            store
                .conn
                .execute(
                    "UPDATE sandbox_reclaim_sweeps SET cycle=0 WHERE sweep_id=1",
                    []
                )
                .is_err()
        );
        assert!(
            store
                .conn
                .execute(
                    "UPDATE sandbox_reclaim_sweeps SET upper_session_id=?1 WHERE sweep_id=1",
                    [Uuid::new_v4().to_string()],
                )
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("DELETE FROM sandbox_reclaim_sweeps WHERE sweep_id=1", [])
                .is_err()
        );
    }

    #[test]
    fn target_reclaim_sweep_bounds_raw_rows_and_custody_lookups_before_later_candidate() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-raw-bound");
        let mut store = Store::open_in_memory().unwrap();
        for ordinal in 0..40 {
            seed_terminal_without_custody(&store, fixture.path(), ordinal);
        }
        let eligible = seed_candidate(&mut store, fixture.path(), 50);

        for _ in 0..5 {
            let page = store.reserve_terminal_reclaim_page(8).unwrap();
            assert!(page.candidates.is_empty());
            assert_eq!(page.evidence.inspected_terminal_rows, 8);
            assert_eq!(page.evidence.custody_lookups, 8);
            assert!(page.has_more);
        }
        let later = store.reserve_terminal_reclaim_page(8).unwrap();
        assert_eq!(later.evidence.inspected_terminal_rows, 1);
        assert_eq!(later.evidence.custody_lookups, 1);
        assert_eq!(later.candidates.len(), 1);
        assert_eq!(later.candidates[0].session_id, eligible);
        assert!(!later.has_more);
    }

    fn seed_intent(store: &mut Store, fixture: &Path, ordinal: u32) -> TargetReclaimIntent {
        seed_candidate(store, fixture, ordinal);
        store
            .prepare_target_reclaim_intent(
                Uuid::from_u128(u128::from(ordinal) + 10_000),
                1,
                1,
                u64::from(ordinal) + 1,
            )
            .unwrap()
            .active()
    }

    #[test]
    fn prepared_recovery_page_wraps_past_stuck_gate_and_skips_staged_rows() {
        let fixture = FixtureDirectory::create("prepared-recovery-page");
        let mut store = Store::open_in_memory().unwrap();
        let first = seed_intent(&mut store, fixture.path(), 0);
        let staged = seed_intent(&mut store, fixture.path(), 1);
        store.mark_target_reclaim_staged(&staged).unwrap();
        let third = seed_intent(&mut store, fixture.path(), 2);

        let first_page = store.prepared_target_reclaim_intent_page(0, 1).unwrap();
        assert_eq!(first_page, vec![first.clone()]);
        let second_page = store
            .prepared_target_reclaim_intent_page(first.schedule_id, 1)
            .unwrap();
        assert_eq!(second_page, vec![third.clone()]);
        let wrapped = store
            .prepared_target_reclaim_intent_page(third.schedule_id, 1)
            .unwrap();
        assert_eq!(wrapped, vec![first]);
    }

    #[test]
    fn prepared_reclaim_blocks_effect_and_tombstone_then_staged_allows_effect() {
        let fixture = FixtureDirectory::create("prepared-reclaim-gate");
        let mut store = Store::open_in_memory().unwrap();
        let prepared = seed_intent(&mut store, fixture.path(), 0);
        let boot = Uuid::new_v4();

        let effect = store
            .reserve_effect(prepared.custody_id, prepared.generation, boot)
            .unwrap_err();
        assert!(effect.to_string().contains("reclaim_prepared"));
        let tombstone = store
            .tombstone_custody_root(
                prepared.custody_id,
                prepared.generation,
                CustodyCause::Purge,
            )
            .unwrap_err();
        assert!(tombstone.to_string().contains("reclaim_prepared"));

        let staged = store.mark_target_reclaim_staged(&prepared).unwrap();
        let reservation = store
            .reserve_effect(staged.custody_id, staged.generation, boot)
            .unwrap();
        store.settle_effect(reservation, false).unwrap();
        let deleting = store.mark_target_reclaim_deleting(&staged).unwrap();
        let reservation = store
            .reserve_effect(deleting.custody_id, deleting.generation, boot)
            .unwrap();
        store.settle_effect(reservation, false).unwrap();
    }

    #[test]
    fn unbound_starting_successor_prevents_prepared_reclaim() {
        let fixture = FixtureDirectory::create("successor-before-reclaim");
        let mut store = Store::open_in_memory().unwrap();
        let owner = seed_candidate(&mut store, fixture.path(), 0);
        let mut child = make_test_session();
        child.id = Uuid::new_v4();
        child.status = SessionStatus::Starting;
        child.continued_from = Some(owner);
        child.working_dir = fixture.path().join("repository");
        child.sandbox_kind = Some(SandboxKind::GitWorktree);
        child.sandbox_root = Some(fixture.path().join("sandbox-0"));
        child.sandbox_branch = Some("rsi/v119-0".into());
        child.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store.insert_session(&child).unwrap();

        assert_eq!(
            store
                .prepare_target_reclaim_intent(Uuid::from_u128(10_000), 1, 1, 1)
                .unwrap(),
            PrepareTargetReclaimIntentResult::SuccessorReservationPending
        );
        assert!(
            store
                .target_reclaim_intent(Uuid::from_u128(10_000), 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn prepared_reclaim_refuses_retry_and_rotation_before_successor_insert() {
        use super::super::daemon_settings::{
            AutofileCause, C5TransitionError, c5_autofile_pending_key,
        };

        let fixture = FixtureDirectory::create("prepared-successor-reservations");
        let mut store = Store::open_in_memory().unwrap();
        let owner = seed_candidate(&mut store, fixture.path(), 0);
        store
            .update_failed_and_stage_c5_autofile(owner, AutofileCause::NonZeroExit)
            .unwrap();
        let marker_key = c5_autofile_pending_key(owner);
        let marker = store.get_c5_autofile_pending(&marker_key).unwrap().unwrap();
        let prepared = store
            .prepare_target_reclaim_intent(Uuid::from_u128(10_000), 1, 1, 1)
            .unwrap()
            .active();
        let mut child = make_test_session();
        child.id = Uuid::new_v4();
        child.status = SessionStatus::Starting;
        child.continued_from = Some(owner);
        child.working_dir = fixture.path().join("repository");
        child.sandbox_kind = Some(SandboxKind::GitWorktree);
        child.sandbox_root = Some(fixture.path().join("sandbox-0"));
        child.sandbox_branch = Some("rsi/v119-0".into());
        child.sandbox_cleanup_state = Some(SandboxCleanupState::Live);

        let retry = store
            .admit_c5_retry_successor(owner, &child, 0, 2, Uuid::new_v4(), &marker)
            .unwrap_err();
        assert!(
            matches!(retry, C5TransitionError::ReclaimPrepared { session_id } if session_id == owner)
        );
        assert_eq!(
            store.get_session(owner).unwrap().unwrap().status,
            SessionStatus::Failed
        );
        assert!(store.get_session(child.id).unwrap().is_none());
        assert_eq!(
            store.get_c5_autofile_pending(&marker_key).unwrap(),
            Some(marker)
        );

        let rotation = store
            .insert_reserved_rotation_session_with_invocation(
                &child,
                Uuid::new_v4(),
                "test-rotation",
            )
            .unwrap_err();
        assert!(rotation.to_string().contains("reclaim_prepared"));
        assert!(store.get_session(child.id).unwrap().is_none());

        let staged = store.mark_target_reclaim_staged(&prepared).unwrap();
        let tx = rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        assert!(
            !super::super::sandbox_custody::prepared_reclaim_for_successor_on(
                &tx,
                owner,
                child.sandbox_root.as_ref().unwrap(),
                child.sandbox_branch.as_deref().unwrap(),
            )
            .unwrap()
        );
        drop(tx);
        assert_eq!(staged.state, TargetReclaimIntentState::Staged);
    }

    #[test]
    fn target_reclaim_intent_scheduler_crosses_two_pages_after_reopen_and_defers_new_tail() {
        let fixture = FixtureDirectory::create("target-reclaim-intent-pages");
        let database = fixture.path().join("intents.db");
        let mut store = Store::open(&database).unwrap();
        let expected = (0..7)
            .map(|ordinal| seed_intent(&mut store, fixture.path(), ordinal).schedule_id)
            .collect::<Vec<_>>();

        let first = store.reserve_target_reclaim_intent_page(2).unwrap();
        assert_eq!(
            first
                .intents
                .iter()
                .map(|intent| intent.schedule_id)
                .collect::<Vec<_>>(),
            expected[..2]
        );
        let frozen_upper = first.evidence.upper_bound;
        drop(store);

        let mut reopened = Store::open(&database).unwrap();
        let inserted_late = seed_intent(&mut reopened, fixture.path(), 8).schedule_id;
        for chunk in expected[2..].chunks(2) {
            let page = reopened.reserve_target_reclaim_intent_page(2).unwrap();
            assert_eq!(page.evidence.upper_bound, frozen_upper);
            assert_eq!(
                page.intents
                    .iter()
                    .map(|intent| intent.schedule_id)
                    .collect::<Vec<_>>(),
                chunk
            );
        }
        let wrap = reopened.reserve_target_reclaim_intent_page(2).unwrap();
        assert!(wrap.intents.is_empty());
        assert!(wrap.evidence.wrapped);
        let next = reopened.reserve_target_reclaim_intent_page(8).unwrap();
        assert!(
            next.intents
                .iter()
                .any(|intent| intent.schedule_id == inserted_late)
        );
    }

    #[test]
    fn pending_intent_survives_fail_closed_custody_lifecycle_transition() {
        let fixture = FixtureDirectory::create("target-reclaim-intent-lifecycle");
        let mut store = Store::open_in_memory().unwrap();
        let intent = seed_intent(&mut store, fixture.path(), 0);

        store
            .record_failed_revalidation(
                intent.custody_id,
                intent.generation,
                SandboxCustodyErrorCodeV1::RootIdentityMismatch,
                SandboxCustodyTransitionV1::EffectRevalidation,
            )
            .unwrap();

        assert_eq!(
            store
                .target_reclaim_intent(intent.custody_id, intent.generation)
                .unwrap(),
            Some(intent.clone())
        );
        assert_eq!(store.prepared_target_reclaim_owner(&intent).unwrap(), None);
        let page = store.reserve_target_reclaim_intent_page(1).unwrap();
        assert_eq!(page.intents, vec![intent]);
    }

    #[test]
    fn target_reclaim_intent_fsm_retains_terminal_evidence_and_cas_fences() {
        let fixture = FixtureDirectory::create("target-reclaim-intent-fsm");
        let mut store = Store::open_in_memory().unwrap();
        let prepared = seed_intent(&mut store, fixture.path(), 0);
        assert!(
            store
                .conn
                .execute(
                    "INSERT INTO sandbox_target_reclaim_intent_events(
                        schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                        payload_name,expected_device,expected_inode,terminal_state,reason,
                        final_row_version,recorded_at)
                     VALUES(?1,?2,?3,?4,?5,?6,'payload',?7,?8,'Abandoned','forged',?9,?10)",
                    params![
                        prepared.schedule_id as i64,
                        prepared.custody_id.to_string(),
                        prepared.generation as i64,
                        prepared.allocation_id.to_string(),
                        i64::from(prepared.bucket),
                        prepared.slot_name.as_str(),
                        prepared.expected_device as i64,
                        prepared.expected_inode.saturating_add(1) as i64,
                        prepared.row_version as i64,
                        Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                    ],
                )
                .is_err()
        );
        let stale = prepared.clone();
        let staged = store.mark_target_reclaim_staged(&prepared).unwrap();
        assert!(store.mark_target_reclaim_staged(&stale).is_err());
        let deleting = store.mark_target_reclaim_deleting(&staged).unwrap();
        store.complete_target_reclaim_intent(&deleting).unwrap();
        assert!(
            store
                .target_reclaim_intent(deleting.custody_id, deleting.generation)
                .unwrap()
                .is_none()
        );

        let abandoned = seed_intent(&mut store, fixture.path(), 1);
        let abandoned_identity = abandoned.clone();
        store
            .abandon_target_reclaim_intent(&abandoned, "custody_revalidation_drift")
            .unwrap();
        let revisited = store
            .prepare_target_reclaim_intent(
                abandoned_identity.custody_id,
                abandoned_identity.generation,
                abandoned_identity.expected_device,
                abandoned_identity.expected_inode,
            )
            .unwrap();
        let PrepareTargetReclaimIntentResult::Terminal(event) = revisited else {
            panic!("terminal candidate revisit recreated an active intent");
        };
        assert_eq!(event.schedule_id, abandoned_identity.schedule_id);
        assert_eq!(event.custody_id, abandoned_identity.custody_id);
        assert_eq!(event.generation, abandoned_identity.generation);
        assert_eq!(event.allocation_id, abandoned_identity.allocation_id);
        assert_eq!(event.bucket, abandoned_identity.bucket);
        assert_eq!(event.slot_name, abandoned_identity.slot_name);
        assert_eq!(event.expected_device, abandoned_identity.expected_device);
        assert_eq!(event.expected_inode, abandoned_identity.expected_inode);
        assert_eq!(
            event.terminal_state,
            TargetReclaimIntentTerminalState::Abandoned
        );
        assert!(
            store
                .target_reclaim_intent(abandoned_identity.custody_id, abandoned_identity.generation)
                .unwrap()
                .is_none()
        );
        let counts = store.target_reclaim_intent_counts().unwrap();
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.abandoned, 1);
        assert!(
            store
                .conn
                .execute(
                    "UPDATE sandbox_target_reclaim_intent_events SET reason='changed'",
                    [],
                )
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("DELETE FROM sandbox_target_reclaim_intent_events", [])
                .is_err()
        );
    }

    #[test]
    fn target_reclaim_sweep_clamps_future_observation_and_rejects_non_rfc3339_cursor() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-clock");
        let mut store = Store::open_in_memory().unwrap();
        seed_candidate(&mut store, fixture.path(), 0);
        seed_candidate(&mut store, fixture.path(), 1);
        seed_candidate(&mut store, fixture.path(), 2);
        let first = store.reserve_terminal_reclaim_page(1).unwrap();
        let second_key = TerminalReclaimSweepKey {
            updated_at: "2026-09-13T12:01:00+00:00".into(),
            session_id: Uuid::from_u128(2),
        };
        let future = "2999-01-01T00:00:00.000000000Z";
        store
            .conn
            .execute(
                "UPDATE sandbox_reclaim_sweeps
                 SET after_updated_at=?1,after_session_id=?2,updated_at=?3
                 WHERE sweep_id=1",
                params![
                    second_key.updated_at,
                    second_key.session_id.to_string(),
                    future
                ],
            )
            .unwrap();

        let third = store.reserve_terminal_reclaim_page(1).unwrap();
        assert_eq!(third.candidates[0].session_id, Uuid::from_u128(3));
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT updated_at FROM sandbox_reclaim_sweeps WHERE sweep_id=1",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            future
        );
        assert!(
            store
                .reserve_terminal_reclaim_page(1)
                .unwrap()
                .evidence
                .wrapped
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT updated_at FROM sandbox_reclaim_sweeps WHERE sweep_id=1",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            future
        );

        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT rsi_rfc3339_is_valid(?1)",
                    [second_key.updated_at],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT rsi_rfc3339_is_valid('2026-09-13 12:00:00.123456')",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(first.evidence.upper_bound.is_some());
    }

    #[test]
    fn target_reclaim_sweep_v119_migration_faults_rollback_retry_and_reopen() -> anyhow::Result<()>
    {
        for fault in TargetReclaimSweepV119MigrationFault::ALL {
            let fixture = FixtureDirectory::create("target-reclaim-sweep-migration");
            let database = fixture.path().join("migration.db");
            let store = Store::open(&database).unwrap();
            rewind_store_to_schema_version(&store.conn, 118);
            assert_eq!(fingerprint(&store.conn), V118_FULL_CATALOG_FINGERPRINT);
            let before = fingerprint(&store.conn);
            fail_next_v119_migration(fault);
            let error = store
                .apply_target_reclaim_sweep_v119_migration()
                .expect_err("V119 failpoint must roll back transaction");
            assert!(error.to_string().contains("injected V119"), "{error}");
            assert_eq!(fingerprint(&store.conn), before);
            assert_eq!(
                store
                    .conn
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                    .unwrap(),
                118
            );
            store.apply_target_reclaim_sweep_v119_migration().unwrap();
            assert_eq!(fingerprint(&store.conn), V119_FULL_CATALOG_FINGERPRINT);
            drop(store);
            let reopened = Store::open(&database).unwrap();
            assert_eq!(
                reopened
                    .conn
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))?,
                crate::store::LATEST_SCHEMA_VERSION
            );
            validate_v119_catalog(&reopened.conn)?;
            crate::store::source_worktree_v120::validate_v120_catalog(&reopened.conn)?;
            crate::store::manager_review_v121::validate_v121_catalog(&reopened.conn)?;
        }
        Ok(())
    }

    #[test]
    fn target_reclaim_sweep_limits_and_catalog_indexes_are_bounded() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.preview_terminal_reclaim_page(0).is_err());
        assert!(
            store
                .reserve_terminal_reclaim_page(MAX_PAGE_SIZE + 1)
                .is_err()
        );
        validate_v119_catalog(&store.conn).unwrap();
        let plan = store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id,updated_at FROM sessions
                 WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                 ORDER BY updated_at,id LIMIT 32",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" ");
        assert!(
            plan.contains("idx_sessions_terminal_reclaim_page_v119"),
            "terminal sweep must use bounded page index: {plan}"
        );

        for (sql, parameters) in [
            (
                SELECT_TERMINAL_KEYS_FROM_START_SQL,
                params![
                    "2026-09-13T12:00:02+00:00",
                    Uuid::from_u128(3).to_string(),
                    32_i64
                ],
            ),
            (
                SELECT_TERMINAL_KEYS_AFTER_SQL,
                params![
                    "2026-09-13T12:00:00+00:00",
                    Uuid::from_u128(1).to_string(),
                    "2026-09-13T12:00:02+00:00",
                    Uuid::from_u128(3).to_string(),
                    32_i64
                ],
            ),
        ] {
            let query_plan = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(parameters, |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join(" ");
            assert!(
                query_plan.contains("idx_sessions_terminal_reclaim_page_v119"),
                "production terminal page must use V119 keyset index: {query_plan}"
            );
        }

        let lookup_plan = store
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {SELECT_CANDIDATE_FOR_KEY_SQL}"
            ))
            .unwrap()
            .query_map(
                params![Uuid::from_u128(1).to_string(), "2026-09-13T12:00:00+00:00"],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" ");
        assert!(
            lookup_plan.contains("sqlite_autoindex_sandbox_custody_roots_3 (owner_session_id=?)")
                && (lookup_plan.contains(
                    "idx_sessions_sandbox_custody_id_id (sandbox_custody_id=? AND id=?)"
                ) || lookup_plan
                    .contains("idx_swc_v120_custody_participants (sandbox_custody_id=? AND id=?)"))
                && !lookup_plan.contains("SCAN"),
            "custody lookup must use bounded custody indexes: {lookup_plan}"
        );
    }

    #[test]
    fn target_reclaim_sweep_write_contention_is_bounded_and_restores_timeout() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-busy");
        let database = fixture.path().join("reclaim.db");
        let mut store = Store::open(&database).unwrap();
        seed_candidate(&mut store, fixture.path(), 0);
        let blocker = Connection::open(&database).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let error = store
            .reserve_terminal_reclaim_page(1)
            .expect_err("competing writer must return bounded contention");
        assert!(
            matches!(
                error,
                DaemonError::Database(rusqlite::Error::SqliteFailure(failure, _))
                    if matches!(
                        failure.code,
                        rusqlite::ErrorCode::DatabaseBusy
                            | rusqlite::ErrorCode::DatabaseLocked
                    )
            ),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "sweep contention exceeded bounded wait: {:?}",
            started.elapsed()
        );
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, u64>(0))
                .unwrap(),
            10_000
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT cycle,after_updated_at FROM sandbox_reclaim_sweeps WHERE sweep_id=1",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .unwrap(),
            (0, None)
        );

        blocker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            store
                .reserve_terminal_reclaim_page(1)
                .unwrap()
                .candidates
                .len(),
            1
        );
    }
}
